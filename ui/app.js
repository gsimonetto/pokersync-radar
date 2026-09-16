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
    hint: "O agente já varre sozinho as pastas padrão de cada sala instalada. Se sua hand history fica num lugar diferente, adicione a pasta abaixo.",
    dialogTitle: "Escolher pasta de hand history",
  },
  tournaments: {
    title: "Importar torneios",
    hint: "Resumo de torneio (buy-in, colocação e premiação) — arquivo separado da hand history. Mesma ideia: o agente já procura nas pastas padrão, adicione outras se precisar.",
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
let lastAutoSyncAt = null;

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
  el("screen-login").classList.toggle("hidden", loggedIn);
  el("screen-app").classList.toggle("hidden", !loggedIn);
}

async function refreshConfig() {
  const cfg = await invoke("get_config");
  el("device-name-input").value = cfg.device_name ?? "";
  el("auto-sync-toggle").checked = cfg.auto_sync_enabled;
  extraFolders = cfg.extra_folders ?? {};
  showScreen(cfg.logged_in);
  if (cfg.logged_in) {
    el("user-email").textContent = cfg.user_email ?? "(sem email)";
    el("account-avatar").textContent = (cfg.user_email ?? "?").trim().charAt(0).toUpperCase();
  }
  renderAutoSyncStatus();
  return cfg;
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
    await refreshConfig();
    await refreshAutostart();
    await loadRooms();
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// ---------- Login com Google (abre o navegador do sistema) ----------

el("btn-google-login").addEventListener("click", async () => {
  const status = el("login-status");
  setStatus(status, "Abrindo o navegador...");
  try {
    await invoke("start_google_login");
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// Caminho manual: o SO nem sempre sabe abrir radar-pokersync:// sozinho
// (varia por SO/instalação) — sem isso, quem confirma no Google e o app
// não reabre fica travado sem nenhuma saída.
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
    await refreshConfig();
    await refreshAutostart();
    await loadRooms();
  } else {
    setStatus(status, event.payload?.error ?? "Não foi possível entrar com o Google.", "err");
  }
});

// ---------- Sync automático em background ----------
// O agente sincroniza sozinho a cada poucos minutos (ver spawn_auto_sync
// em src-tauri/src/lib.rs); este evento só avisa a UI quando um ciclo
// realmente importou algo, pra "última sincronização" não ficar mentindo.
listen("auto-sync-result", (event) => {
  lastAutoSyncAt = Date.now();
  renderAutoSyncStatus(event.payload?.imported ?? 0);
});

function renderAutoSyncStatus(justImported) {
  const node = el("auto-sync-status");
  if (!el("auto-sync-toggle").checked) {
    node.textContent = "Sincronização automática desligada — ative em Configurações.";
    node.classList.add("is-off");
    return;
  }
  node.classList.remove("is-off");
  if (justImported) {
    node.textContent = `Sincronizado automaticamente agora — ${justImported} mão(s)/torneio(s) novo(s).`;
    return;
  }
  node.textContent = "Sincronização automática ativa — roda sozinha em background, sem precisar clicar em nada.";
}

// ---------- Auto-update ----------
// Nunca baixa/instala sozinho: só avisa com um banner e espera o
// jogador clicar em "Atualizar agora" -- ele pode estar no meio de uma
// sessão, e instalar reinicia o app. `window.__TAURI__.updater` e
// `.process` vêm dos plugins tauri-plugin-updater/tauri-plugin-process
// (ver src-tauri/src/lib.rs) -- mesmo padrão de window.__TAURI__.dialog
// já usado aqui, funcionando graças a "withGlobalTauri": true.
let pendingUpdate = null;

async function checkForUpdate() {
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
    renderAutoSyncStatus();
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
  el("email").value = "";
  el("password").value = "";
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
    setStatus(el("config-status"), String(err), "err");
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

// "Verificar agora" é um atalho opcional pra feedback imediato — a
// sincronização de verdade já roda sozinha em background (ver
// spawn_auto_sync), então ninguém É OBRIGADO a clicar aqui.
el("btn-import-scan").addEventListener("click", async () => {
  const status = el("import-scan-status");
  setStatus(status, "Verificando e sincronizando...");
  try {
    const summaries = await invoke("sync_now", { kind: openImportKind });
    renderImportResults(
      summaries.map((s) => ({
        room: roomLabel(s.room),
        files: s.files_synced,
        detail: s.files_synced > 0 ? `${s.imported} nova(s), ${s.duplicates} duplicada(s), ${s.errors} c/ erro` : "tudo sincronizado",
      }))
    );
    const total = summaries.reduce((acc, s) => acc + s.imported, 0);
    setStatus(status, `Verificação concluída — ${total} novo(s).`, "ok");
  } catch (err) {
    setStatus(status, String(err), "err");
  }
});

// Só decide login-vs-app quando o splash termina (finishSplash chama
// boot()) — chamar refreshConfig() antes disso destravaria login/app por
// baixo do vídeo, já que showScreen() tira a classe "hidden" na hora.
async function boot() {
  const cfg = await refreshConfig();
  if (cfg.logged_in) {
    await refreshAutostart();
    await loadRooms();
    checkForUpdate(); // não bloqueia o boot -- só avisa quando terminar
  }
}
