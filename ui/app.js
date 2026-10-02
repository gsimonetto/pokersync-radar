const invoke = window.__TAURI__.core.invoke;
const openFolderDialog = window.__TAURI__.dialog.open;
const listen = window.__TAURI__.event.listen;

const el = (id) => document.getElementById(id);

// Cores de acento — do próprio design system do PokerSync (não são as
// cores de marca de cada sala; ver decisão em ui/README caso exista).
// Cada sala recebe um acento fixo só pra diferenciar visualmente os
// cards, com um badge de iniciais no lugar de logotipos de terceiros.
const ROOM_STYLE = {
  pokerstars: { initials: "PS", accent: "#3b82f6" },
  ggpoker: { initials: "GG", accent: "#f59e0b" },
  partypoker: { initials: "PP", accent: "#a855f7" },
  "888poker": { initials: "888", accent: "#22c55e" },
  acr: { initials: "ACR", accent: "#e0555a" },
};

const IMPORT_KIND_META = {
  hands: {
    title: "Importar mãos",
    hint: "O Radar já varre sozinho as pastas padrão de cada sala instalada. Se sua hand history fica num lugar diferente, adicione a pasta abaixo.",
    dialogTitle: "Escolher pasta de hand history",
  },
  tournaments: {
    title: "Importar torneios",
    hint: "Resumo de torneio (buy-in, colocação e premiação) — arquivo separado da hand history. Mesma ideia: o Radar já procura nas pastas padrão, adicione outras se precisar.",
    dialogTitle: "Escolher pasta de resumo de torneio",
  },
};

function setStatus(node, message, kind) {
  node.innerHTML = "";
  if (!message) return;
  // O ícone é sempre um dos dois SVGs fixos abaixo (nunca monta com dado
  // variável) — só `message` é dinâmico (pode vir de erro do servidor ou
  // rede), por isso vai por `textContent`, nunca por innerHTML: uma
  // resposta de erro maliciosa não pode virar HTML/JS executado aqui.
  const icon =
    kind === "err"
      ? '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="10"/><path d="M12 8v5M12 16h.01"/></svg>'
      : kind === "ok"
        ? '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><path d="m22 4-10 10-3-3"/></svg>'
        : "";
  node.className = "status-msg" + (kind ? " " + kind : "");
  node.innerHTML = icon;
  const span = document.createElement("span");
  span.textContent = message;
  node.appendChild(span);
}

let rooms = [];
let extraFolders = {};
let openImportKind = null;
let ultimoStatus = null;

// ---------- Splash (vídeo antes do login) ----------
// Só toca uma vez por processo — a janela do Tauri nunca é destruída
// (fechar minimiza pra bandeja, ver src-tauri/src/lib.rs), então este
// script só roda de novo se o app for reiniciado de verdade.
let splashDone = false;
function finishSplash() {
  if (splashDone) return;
  splashDone = true;
  el("screen-splash").classList.add("hidden");
  boot();
}
el("splash-video").addEventListener("ended", finishSplash);
// Autoplay pode falhar (política do WebView) ou o arquivo pode não
// carregar — nenhum dos dois pode travar quem só quer logar.
el("splash-video").addEventListener("error", finishSplash);
el("btn-splash-skip").addEventListener("click", finishSplash);
// Rede de segurança: nunca prende a tela de login por mais que alguns
// segundos, mesmo se "ended" nunca disparar por algum motivo.
setTimeout(finishSplash, 8000);

function showScreen(loggedIn) {
  if (!splashDone) return;
  el("screen-login").classList.toggle("hidden", loggedIn);
  el("screen-app").classList.toggle("hidden", !loggedIn);
}

async function refreshConfig() {
  const cfg = await invoke("get_config");
  el("device-name-input").value = cfg.device_name ?? "";
  el("auto-sync-toggle").checked = cfg.auto_sync_enabled;
  extraFolders = cfg.extra_folders ?? {};
  showScreen(cfg.logged_in);
  el("login-aviso").classList.toggle("hidden", cfg.logged_in || !cfg.sessao_expirada);
  if (cfg.logged_in) {
    el("user-email").textContent = cfg.user_email ?? "(sem email)";
    el("account-avatar").textContent = (cfg.user_email ?? "?").trim().charAt(0).toUpperCase();
  }
  if (ultimoStatus) renderStatus(ultimoStatus);
  return cfg;
}

// Depois de qualquer login que deu certo (senha ou Google).
async function entrouNaConta() {
  await refreshConfig();
  await refreshAutostart();
  await loadRooms();
  carregarSalas();
  checkForUpdate();
}

// ---------- Login (email/senha) ----------

el("toggle-password").addEventListener("click", () => {
  const input = el("password");
  input.type = input.type === "password" ? "text" : "password";
});

el("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const status = el("login-status");
  setStatus(status, "Entrando...");
  try {
    await invoke("login", { email: el("email").value, password: el("password").value });
    setStatus(status, "", null);
    el("password").value = "";
    await entrouNaConta();
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// ---------- Login com Google (abre o navegador do sistema) ----------

el("btn-google-login").addEventListener("click", async () => {
  const status = el("login-status");
  setStatus(status, "Abrindo o navegador... depois de entrar lá, o Radar volta sozinho pra cá.");
  try {
    await invoke("start_google_login");
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// Caminho manual: se mesmo assim o navegador não abrir o Radar sozinho
// (varia por SO/instalação), a página de conclusão mostra um link pra
// copiar e colar aqui.
el("btn-show-paste-link").addEventListener("click", () => {
  el("paste-link-row").classList.remove("hidden");
  el("btn-show-paste-link").classList.add("hidden");
  el("paste-link-input").focus();
});

el("btn-paste-link-confirm").addEventListener("click", async () => {
  const status = el("login-status");
  const link = el("paste-link-input").value.trim();
  if (!link) return;
  setStatus(status, "Confirmando login...");
  try {
    await invoke("paste_login_link", { link });
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

listen("google-login-result", async (event) => {
  const status = el("login-status");
  if (event.payload?.ok) {
    setStatus(status, "", null);
    el("paste-link-input").value = "";
    await entrouNaConta();
  } else {
    setStatus(status, event.payload?.error ?? "Não foi possível entrar com o Google.", "err");
  }
});

// ---------- Situação do Radar ----------
// O ciclo em background (spawn_ciclo_automatico em src-tauri/src/lib.rs)
// confere o login, manda o sinal de vida pro site e envia o que é novo —
// e avisa cada mudança pelo evento "status-changed".

function tempoDesde(segundos) {
  if (!segundos) return "";
  const d = Math.max(0, Math.floor(Date.now() / 1000) - segundos);
  if (d < 60) return "agora há pouco";
  const min = Math.floor(d / 60);
  if (min < 60) return `há ${min} min`;
  const h = Math.floor(min / 60);
  if (h < 24) return `há ${h} h`;
  const dias = Math.floor(h / 24);
  return `há ${dias} ${dias === 1 ? "dia" : "dias"}`;
}

const AVISOS = {
  plano: {
    texto: "Seu plano não inclui o Radar PokerSync — por isso nenhuma mão está sendo enviada.",
    botao: "Ver planos",
    destino: "planos",
  },
};
let avisoAtual = null;
let ultimaVerificacaoVista = null;

// ---------- Pergunta "o que importar" ----------
// Pedido explícito: a escolha fica no Radar e no site (é a mesma). Aparece
// enquanto o jogador não respondeu, com os números do que o Radar achou
// no computador — contados uma vez só (lê os arquivos, pode demorar uns
// segundos num histórico grande).
let resumoPedido = false;
const numero = (n) => Number(n).toLocaleString("pt-BR");

async function carregarResumo() {
  if (resumoPedido) return;
  resumoPedido = true;
  try {
    const r = await invoke("resumo_do_computador");
    if (r.maos === 0 && r.torneios === 0) {
      el("escolha-resumo").textContent =
        "Ainda não achei mãos no seu computador — o Radar procura nas pastas padrão de cada sala (dá pra adicionar outras em \"Importar mãos\").";
      return;
    }
    el("escolha-resumo").textContent =
      `Achei ${numero(r.maos)} mãos e ${numero(r.torneios)} torneios no seu computador` +
      (r.mais_antigo ? `, desde ${r.mais_antigo}.` : ".");
    document.querySelector('[data-texto="last_3_months"]').textContent =
      `${numero(r.maos_ultimos_3_meses)} mãos e ${numero(r.torneios_ultimos_3_meses)} torneios dos últimos 3 meses, e tudo daqui pra frente.`;
    document.querySelector('[data-texto="full_history"]').textContent =
      `Todas as ${numero(r.maos)} mãos e ${numero(r.torneios)} torneios que estão no computador, e tudo daqui pra frente.`;
  } catch {
    el("escolha-resumo").textContent = "Escolha uma opção — dá pra trocar depois.";
  }
}

document.querySelectorAll(".escolha-opcao").forEach((botao) => {
  botao.addEventListener("click", async () => {
    const erro = el("escolha-erro");
    erro.classList.add("hidden");
    document.querySelectorAll(".escolha-opcao").forEach((b) => (b.disabled = true));
    try {
      renderStatus(await invoke("escolher_importacao", { escopo: botao.dataset.escopo }));
    } catch (err) {
      setStatus(erro, String(err), "err");
      erro.classList.remove("hidden");
    } finally {
      document.querySelectorAll(".escolha-opcao").forEach((b) => (b.disabled = false));
    }
  });
});

function renderStatus(s) {
  ultimoStatus = s;
  if (s.conexao === "sessao_expirada") {
    // Sessão acabou: volta pra tela de login explicando o motivo.
    showScreen(false);
    el("login-aviso").classList.remove("hidden");
    return;
  }
  const autoLigado = el("auto-sync-toggle").checked;
  let tom = "ok";
  let titulo;
  let detalhe = "";

  if (s.conexao === "conectando") {
    tom = "neutro";
    titulo = "Conectando ao PokerSync…";
  } else if (s.conexao === "desconectado") {
    tom = "neutro";
    titulo = "Fora da conta";
  } else if (s.conexao === "sem_internet") {
    tom = "alerta";
    titulo = "Sem conexão com o PokerSync";
    detalhe = "O Radar tenta de novo sozinho em instantes.";
  } else if (s.radar_liberado === false) {
    tom = "alerta";
    titulo = "Radar pausado";
  } else if (!s.import_scope) {
    tom = "alerta";
    titulo = "Falta só escolher o que trazer";
  } else if (s.sincronizando) {
    titulo = "Sincronizando…";
  } else if (!autoLigado) {
    tom = "neutro";
    titulo = "Sincronização automática desligada";
    detalhe = "Ligue em Configurações, ou use \"Sincronizar agora\".";
  } else if (s.pastas_vigiadas > 0) {
    // As pastas das salas estão sendo vigiadas: a mão vai pro PokerSync
    // segundos depois de jogada (ver src-tauri/src/watcher.rs).
    titulo = "Tudo certo — sincronizando em tempo real";
  } else {
    titulo = "Tudo certo — sincronizando sozinho";
  }

  if (tom === "ok") {
    const partes = [];
    if (s.ultima_sincronizacao) partes.push(`Última verificação ${tempoDesde(s.ultima_sincronizacao)}`);
    if (s.ultimas_novidades) {
      const n = s.ultimas_novidades;
      partes.push(`${n} ${n === 1 ? "novo enviado" : "novos enviados"} ${tempoDesde(s.ultimas_novidades_em)}`);
    }
    // Arquivos que o site recusou: ficam de lado (o Radar tenta de novo em
    // um dia ou quando o arquivo mudar) e o resto segue sendo enviado.
    if (s.arquivos_recusados) {
      const r = s.arquivos_recusados;
      partes.push(`${r} ${r === 1 ? "arquivo recusado" : "arquivos recusados"} pelo PokerSync`);
    }
    detalhe = partes.join(" · ");
  }
  if (s.ultimo_erro && s.conexao !== "sem_internet") {
    tom = "erro";
    detalhe = s.ultimo_erro;
  }

  el("status-card").dataset.tom = tom;
  el("status-titulo").textContent = titulo;
  el("status-detalhe").textContent = detalhe;
  el("btn-sync-agora").disabled = s.sincronizando || s.conexao === "conectando";

  avisoAtual = s.conexao === "ok" && s.radar_liberado === false ? AVISOS.plano : null;
  el("status-aviso").classList.toggle("hidden", !avisoAtual);
  if (avisoAtual) {
    el("status-aviso-texto").textContent = avisoAtual.texto;
    el("btn-status-aviso").textContent = avisoAtual.botao;
  }

  const perguntar = s.conexao === "ok" && s.radar_liberado !== false && !s.import_scope;
  el("escolha-importacao").classList.toggle("hidden", !perguntar);
  if (perguntar) carregarResumo();

  // Depois de cada verificação (automática ou não), os números das salas mudam.
  if (s.ultima_sincronizacao && s.ultima_sincronizacao !== ultimaVerificacaoVista) {
    ultimaVerificacaoVista = s.ultima_sincronizacao;
    carregarSalas();
  }
}

listen("status-changed", (event) => renderStatus(event.payload));
// Mantém o "há X min" certo mesmo sem evento novo.
setInterval(() => ultimoStatus && renderStatus(ultimoStatus), 30000);

el("btn-status-aviso").addEventListener("click", async () => {
  if (!avisoAtual) return;
  try {
    await invoke("abrir_no_site", { destino: avisoAtual.destino });
  } catch (err) {
    el("status-detalhe").textContent = String(err);
  }
});

el("btn-sync-agora").addEventListener("click", async () => {
  const btn = el("btn-sync-agora");
  btn.disabled = true;
  try {
    renderStatus(await invoke("sincronizar_agora"));
  } catch (err) {
    el("status-card").dataset.tom = "erro";
    el("status-detalhe").textContent = String(err);
  } finally {
    btn.disabled = false;
  }
});

// ---------- Suas salas ----------
// Pra cada sala: se a pasta existe, quanto tem nela e o que fazer quando
// não acha nada (painel_salas no lado Rust). Tudo que vem de lá entra por
// textContent — caminho de pasta e nome de arquivo são dados, não HTML.

const SITUACAO = {
  ok: { texto: "Sincronizado", tom: "ok" },
  pendente: { texto: "Enviando", tom: "info" },
  aguardando_site: { texto: "Aguardando o site", tom: "alerta" },
  pasta_vazia: { texto: "Pasta vazia", tom: "alerta" },
  nao_encontrada: { texto: "Não encontrada", tom: "neutro" },
};

let salasCarregando = false;
let salasDeNovo = false;

async function carregarSalas() {
  // Um pedido por vez; se pedirem de novo no meio, refaz uma vez no fim.
  if (salasCarregando) {
    salasDeNovo = true;
    return;
  }
  salasCarregando = true;
  try {
    renderSalas(await invoke("painel_salas"));
  } catch (err) {
    const grid = el("salas-grid");
    grid.innerHTML = "";
    const card = document.createElement("div");
    card.className = "sala-card sala-carregando";
    card.textContent = `Não consegui olhar as pastas agora: ${err}`;
    grid.appendChild(card);
  } finally {
    salasCarregando = false;
    if (salasDeNovo) {
      salasDeNovo = false;
      carregarSalas();
    }
  }
}

// Corta o começo (o fim do caminho é o que diz qual pasta é); curto o
// bastante pra caber no cartão sem o CSS cortar o fim também.
function pastaCurta(pasta) {
  return pasta.length > 40 ? "…" + pasta.slice(-38) : pasta;
}

function renderSalas(salas) {
  const grid = el("salas-grid");
  grid.innerHTML = "";
  // Salas com alguma coisa primeiro; as não encontradas no fim, apagadas.
  const ordem = (s) => (s.situacao === "nao_encontrada" ? 2 : s.situacao === "pasta_vazia" ? 1 : 0);
  for (const sala of [...salas].sort((a, b) => ordem(a) - ordem(b))) {
    const situacao = SITUACAO[sala.situacao] ?? SITUACAO.nao_encontrada;
    const card = document.createElement("div");
    card.className = "sala-card" + (sala.situacao === "nao_encontrada" ? " apagada" : "");

    const topo = document.createElement("div");
    topo.className = "sala-topo";
    const nome = document.createElement("span");
    nome.className = "sala-nome";
    const ponto = document.createElement("span");
    ponto.className = "room-dot";
    ponto.style.background = ROOM_STYLE[sala.slug]?.accent ?? "var(--muted)";
    nome.appendChild(ponto);
    nome.appendChild(document.createTextNode(sala.nome));
    const pill = document.createElement("span");
    pill.className = "sala-pill";
    pill.dataset.tom = situacao.tom;
    pill.textContent = situacao.texto;
    topo.appendChild(nome);
    topo.appendChild(pill);
    card.appendChild(topo);

    if (sala.maos || sala.torneios) {
      const numeros = document.createElement("div");
      numeros.className = "sala-numeros";
      const partes = [`${numero(sala.maos)} arquivo(s) de mãos`, `${numero(sala.torneios)} de torneios`];
      const pendentes = sala.maos_pendentes + sala.torneios_pendentes;
      if (pendentes && sala.situacao !== "aguardando_site") partes.push(`${numero(pendentes)} pra enviar`);
      numeros.textContent = partes.join(" · ");
      card.appendChild(numeros);
    }

    if (sala.pastas.length) {
      const pasta = document.createElement("div");
      pasta.className = "sala-pasta";
      pasta.title = sala.pastas.join("\n");
      pasta.textContent =
        pastaCurta(sala.pastas[0]) + (sala.pastas.length > 1 ? ` (+${sala.pastas.length - 1})` : "");
      card.appendChild(pasta);
    }

    if (sala.dica) {
      const dica = document.createElement("div");
      dica.className = "sala-dica";
      dica.textContent = sala.dica;
      card.appendChild(dica);
    }
    grid.appendChild(card);
  }
}

el("btn-salas-atualizar").addEventListener("click", carregarSalas);

// ---------- Importar arquivos baixados ----------
// O "importar do disco" do PokerTracker/Holdem Manager: o jogador escolhe
// o .zip baixado do PokerCraft (GGPoker) ou .txt soltos; o Radar reconhece
// sala e tipo de cada arquivo e envia (importar_arquivos no lado Rust).

const TIPO_ARQUIVO = { hands: "Mãos", tournaments: "Torneios" };

function resultadoDaLinha(l) {
  if (l.aguardando) return "Aguardando o site aprender a ler essa sala — nada enviado ainda";
  const partes = [`${l.imported} nova(s)`, `${l.duplicates} repetida(s)`];
  if (l.ignored_by_date) partes.push(`${l.ignored_by_date} de antes do corte`);
  if (l.errors) partes.push(`${l.errors} c/ erro`);
  if (l.recusados) partes.push(`${l.recusados} arquivo(s) recusado(s)`);
  return partes.join(", ");
}

el("btn-import-files").addEventListener("click", async () => {
  const escolhidos = await openFolderDialog({
    multiple: true,
    directory: false,
    title: "Escolher arquivos baixados (.zip ou .txt)",
    filters: [{ name: "Histórico de mãos", extensions: ["zip", "txt"] }],
  });
  if (!escolhidos || (Array.isArray(escolhidos) && escolhidos.length === 0)) return;
  const caminhos = Array.isArray(escolhidos) ? escolhidos : [escolhidos];

  el("arquivos-panel").classList.remove("hidden");
  el("arquivos-tabela").classList.add("hidden");
  const status = el("arquivos-status");
  setStatus(status, `Lendo e enviando ${caminhos.length} arquivo(s)…`);
  el("arquivos-panel").scrollIntoView({ behavior: "smooth", block: "nearest" });
  try {
    const r = await invoke("importar_arquivos", { caminhos });
    const corpo = el("arquivos-corpo");
    corpo.innerHTML = "";
    for (const l of r.linhas) {
      const tr = document.createElement("tr");
      const sala = document.createElement("td");
      sala.innerHTML = roomLabel(l.sala); // rótulo montado só com dados fixos (list_rooms/ROOM_STYLE)
      const tipo = document.createElement("td");
      tipo.textContent = TIPO_ARQUIVO[l.tipo] ?? l.tipo;
      const resultado = document.createElement("td");
      resultado.textContent = `${l.arquivos} arquivo(s): ${resultadoDaLinha(l)}`;
      tr.append(sala, tipo, resultado);
      corpo.appendChild(tr);
    }
    el("arquivos-tabela").classList.toggle("hidden", r.linhas.length === 0);
    const novos = r.linhas.reduce((acc, l) => acc + l.imported, 0);
    const ignorados = r.ignorados ? ` ${r.ignorados} arquivo(s) não eram de mão/torneio (ou estavam repetidos) e ficaram de fora.` : "";
    if (r.linhas.length === 0) {
      setStatus(status, `Nenhum arquivo de mão ou torneio reconhecido nos ${r.arquivos_lidos} lido(s).`, "err");
    } else {
      setStatus(status, `Importação concluída — ${novos} novo(s).${ignorados}`, "ok");
    }
    carregarSalas();
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

el("btn-arquivos-panel-close").addEventListener("click", () => {
  el("arquivos-panel").classList.add("hidden");
});

// ---------- Auto-update ----------
// Nunca baixa/instala sozinho: só avisa com um banner e espera o
// jogador clicar em "Atualizar agora" -- ele pode estar no meio de uma
// sessão, e instalar reinicia o app. `window.__TAURI__.updater` e
// `.process` vêm dos plugins tauri-plugin-updater/tauri-plugin-process
// (ver src-tauri/src/lib.rs) -- mesmo padrão de window.__TAURI__.dialog
// já usado aqui, funcionando graças a "withGlobalTauri": true.
let pendingUpdate = null;

async function checkForUpdate() {
  if (pendingUpdate) return;
  try {
    const update = await window.__TAURI__.updater.check();
    if (!update) return;
    pendingUpdate = update;
    el("update-banner-text").textContent = `Nova versão disponível (${update.version}).`;
    el("update-banner").classList.remove("hidden");
  } catch (e) {
    // Sem rede, GitHub fora do ar, etc. -- não incomoda o jogador com
    // erro por causa de uma verificação em background.
    console.error("Falha ao verificar atualização:", e);
  }
}
// O Radar fica ligado dias seguidos — confere de novo a cada 6 horas.
setInterval(checkForUpdate, 6 * 60 * 60 * 1000);

el("btn-update-install").addEventListener("click", async () => {
  if (!pendingUpdate) return;
  const btn = el("btn-update-install");
  btn.disabled = true;
  btn.textContent = "Baixando...";
  try {
    await pendingUpdate.downloadAndInstall();
    await window.__TAURI__.process.relaunch();
  } catch (e) {
    btn.disabled = false;
    btn.textContent = "Atualizar agora";
    setStatus(el("update-banner-text"), "Falha ao atualizar — tente de novo mais tarde.", "err");
  }
});

// ---------- Configurações avançadas (modal) ----------

function openSettings() {
  el("settings-overlay").classList.remove("hidden");
}
function closeSettings() {
  el("settings-overlay").classList.add("hidden");
}

el("btn-settings").addEventListener("click", openSettings);
el("btn-settings-close").addEventListener("click", closeSettings);
el("settings-overlay").addEventListener("click", (e) => {
  if (e.target === e.currentTarget) closeSettings(); // clique fora do card
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !el("settings-overlay").classList.contains("hidden")) closeSettings();
});

el("device-name-input").addEventListener("change", async (e) => {
  try {
    await invoke("save_device_name", { deviceName: e.target.value });
  } catch (err) {
    setStatus(el("config-status"), String(err), "err");
  }
});

el("auto-sync-toggle").addEventListener("change", async (e) => {
  const enabled = e.target.checked;
  try {
    await invoke("set_auto_sync_enabled", { enabled });
    if (ultimoStatus) renderStatus(ultimoStatus);
  } catch (err) {
    e.target.checked = !enabled;
    setStatus(el("config-status"), String(err), "err");
  }
});

el("btn-test").addEventListener("click", async () => {
  const status = el("config-status");
  setStatus(status, "Testando...");
  try {
    const msg = await invoke("test_connection");
    setStatus(status, msg, "ok");
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

el("btn-logout").addEventListener("click", async () => {
  await invoke("logout");
  closeSettings();
  el("email").value = "";
  el("password").value = "";
  setStatus(el("config-status"), "", null);
  await refreshConfig();
});

async function refreshAutostart() {
  el("autostart-toggle").checked = await invoke("get_autostart");
}

el("autostart-toggle").addEventListener("change", async (e) => {
  const enabled = e.target.checked;
  try {
    await invoke("set_autostart", { enabled });
  } catch (err) {
    e.target.checked = !enabled;
    el("status-detalhe").textContent = `Não consegui mudar isso agora: ${err}`;
  }
});

// ---------- Importação (mãos / torneios) ----------
// Antes era "escolha a sala, depois a pasta" — agora são só 2 botões
// (mãos/torneios), cada um varrendo TODAS as salas de uma vez; a sala de
// cada arquivo aparece como informação nos resultados, não como escolha
// prévia. Pastas extras também deixaram de ser por sala — uma pasta
// adicionada aqui é varrida contra todas as salas (ver discover_all no
// lado Rust).

async function loadRooms() {
  rooms = await invoke("list_rooms");
}

function roomLabel(slug) {
  const name = rooms.find((r) => r.slug === slug)?.display_name ?? slug;
  const style = ROOM_STYLE[slug];
  if (!style) return name;
  return `<span class="room-dot" style="background:${style.accent}"></span>${name}`;
}

function renderImportFolders() {
  const list = el("import-folder-list");
  list.innerHTML = "";
  const folders = extraFolders[openImportKind] ?? [];
  if (folders.length === 0) {
    const empty = document.createElement("span");
    empty.className = "folder-chip empty";
    empty.textContent = "só pastas padrão";
    list.appendChild(empty);
    return;
  }
  for (const folder of folders) {
    const chip = document.createElement("span");
    chip.className = "folder-chip";
    chip.title = folder;
    const text = document.createElement("span");
    text.textContent = folder.length > 44 ? "…" + folder.slice(-42) : folder;
    const remove = document.createElement("button");
    remove.textContent = "×";
    remove.className = "chip-remove";
    remove.addEventListener("click", () => removeImportFolder(folder));
    chip.appendChild(text);
    chip.appendChild(remove);
    list.appendChild(chip);
  }
}

async function saveImportFolders() {
  await invoke("save_extra_folders", { kind: openImportKind, folders: extraFolders[openImportKind] ?? [] });
  carregarSalas();
}

el("btn-import-add-folder").addEventListener("click", async () => {
  const meta = IMPORT_KIND_META[openImportKind];
  const picked = await openFolderDialog({ directory: true, multiple: false, title: meta.dialogTitle });
  if (!picked) return;
  const current = extraFolders[openImportKind] ?? [];
  if (!current.includes(picked)) {
    extraFolders[openImportKind] = [...current, picked];
    await saveImportFolders();
  }
  renderImportFolders();
});

async function removeImportFolder(folder) {
  extraFolders[openImportKind] = (extraFolders[openImportKind] ?? []).filter((f) => f !== folder);
  await saveImportFolders();
  renderImportFolders();
}

function openImportPanel(kind) {
  openImportKind = kind;
  const meta = IMPORT_KIND_META[kind];
  el("import-panel-title").textContent = meta.title;
  el("import-panel-hint").textContent = meta.hint;
  el("import-panel").classList.remove("hidden");
  el("import-results-table").classList.add("hidden");
  el("import-scan-status").innerHTML = "";
  renderImportFolders();
  el("import-panel").scrollIntoView({ behavior: "smooth", block: "nearest" });
}

el("btn-open-hands").addEventListener("click", () => openImportPanel("hands"));
el("btn-open-tournaments").addEventListener("click", () => openImportPanel("tournaments"));
el("btn-import-panel-close").addEventListener("click", () => {
  el("import-panel").classList.add("hidden");
  openImportKind = null;
});

function renderImportResults(rows) {
  const table = el("import-results-table");
  const body = el("import-results-body");
  body.innerHTML = "";
  for (const row of rows) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td>${row.room}</td><td>${row.files}</td><td>${row.detail}</td>`;
    body.appendChild(tr);
  }
  table.classList.toggle("hidden", rows.length === 0);
}

function detalheDaSala(s) {
  const recusados = s.files_rejected
    ? `${s.files_rejected} arquivo(s) recusado(s) pelo PokerSync — o Radar tenta de novo amanhã`
    : "";
  if (s.files_waiting) {
    return `${s.files_waiting} arquivo(s) encontrado(s) — o envio começa assim que o PokerSync aprender a ler essa sala`;
  }
  if (s.files_synced === 0) return recusados || "tudo sincronizado";
  const partes = [`${s.imported} nova(s)`, `${s.duplicates} repetida(s)`];
  // Mãos de antes do "só a partir de agora" escolhido no site.
  if (s.ignored_by_date) partes.push(`${s.ignored_by_date} de antes do corte`);
  partes.push(`${s.errors} c/ erro`);
  if (recusados) partes.push(recusados);
  return partes.join(", ");
}

// "Verificar agora" é um atalho opcional pra feedback imediato — a
// sincronização de verdade já roda sozinha em background (ver
// spawn_ciclo_automatico), então ninguém É OBRIGADO a clicar aqui.
el("btn-import-scan").addEventListener("click", async () => {
  const status = el("import-scan-status");
  setStatus(status, "Verificando e sincronizando...");
  try {
    const summaries = await invoke("sync_now", { kind: openImportKind });
    renderImportResults(
      summaries.map((s) => ({
        room: roomLabel(s.room),
        files: s.files_synced,
        detail: detalheDaSala(s),
      }))
    );
    const total = summaries.reduce((acc, s) => acc + s.imported, 0);
    setStatus(status, `Verificação concluída — ${total} novo(s).`, "ok");
    carregarSalas();
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// Só decide login-vs-app quando o splash termina (finishSplash chama
// boot()) — chamar refreshConfig() antes disso destravaria login/app por
// baixo do vídeo, já que showScreen() tira a classe "hidden" na hora.
async function boot() {
  renderStatus(await invoke("get_status"));
  const cfg = await refreshConfig();
  if (cfg.logged_in) {
    await refreshAutostart();
    await loadRooms();
    carregarSalas(); // não bloqueia o boot
    checkForUpdate(); // não bloqueia o boot -- só avisa quando terminar
  }
}
