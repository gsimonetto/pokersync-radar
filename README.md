# Radar PokerSync

Agente desktop (Tauri + Rust) que varre o computador do jogador em busca de
hand histories e torneios jogados — PokerStars, GGPoker, PartyPoker, 888poker
e ACR — e sincroniza com o PokerSync. Implementa a decisão 005 e o item
4 do backlog vivo do produto (ver `POKERSYNC.md` §5/§7 em
[`gsimonetto/pokersync`](https://github.com/gsimonetto/pokersync)).

## Por que assim

- **O agente não faz parsing de mão.** Isso já existe, é validado contra
  hand history real e é bilíngue (EN/PT-BR): `lib/poker/hand-parser.ts`, no
  repo do produto (`gsimonetto/pokersync`). Duplicar essa lógica em Rust
  criaria dois parsers que divergem com o tempo. O agente só encontra
  arquivos, evita reenviar o que não mudou, e manda o texto bruto pro
  backend — quem parseia é o mesmo código que já atende o "colar hand
  history" manual do Revisor.
- **Tauri, não Electron.** Um watcher que roda em background o dia inteiro
  não deveria custar 150MB de RAM parado. WebView nativo do SO + Rust dá um
  binário pequeno e leve pra isso.
- **Repositório próprio.** Mesmo padrão do motor GTO (decisão 009,
  `pokersync-solver`): algo que muda a versão do agente não deve passar
  pelo pipeline de deploy do Next.js, e vice-versa. Extraído do repo do
  produto via `git subtree split` (histórico preservado).

## Estrutura

```
crates/
  scanner/        # descoberta de arquivos + estado de sync (sem parsing de mão)
  sync-client/     # cliente HTTP para /api/agent/{ping,sync}
src-tauri/         # shell Tauri: comandos, config local, login
ui/                # frontend estático (HTML/CSS/JS puro, sem bundler)
```

Do lado do produto, em `gsimonetto/pokersync` (`app/`, `lib/`):

- `app/api/agent/sync/route.ts` — recebe o texto bruto, autentica por
  bearer token (Supabase JWT do usuário).
- `app/api/agent/ping/route.ts` — valida o token e recebe o "sinal de
  vida" do Radar (POST com o aparelho, a cada ciclo); responde o que o
  jogador escolheu importar (`importScope`) e se o plano inclui o Radar
  (`radarLiberado`).
- `app/api/agent/import-scope/route.ts` — salva a escolha do que importar
  feita dentro do Radar (mesma escolha da página do Radar no site).
- `lib/services/agent-sync-service.ts` — parseia (via `hand-parser.ts`),
  deduplica por `external_hand_id` e grava em `hand_reviews` (`source:
  "agent"`), atualizando `hand_sync_devices`/`hand_sync_batches`.

## Como funciona a varredura

1. Pra cada sala selecionada, `PokerRoom::default_search_paths()` lista
   pastas plausíveis por SO (Documents/AppData/Application Support, por
   variação de skin conhecida). **Best-effort** — cada operadora muda isso
   sem aviso. A UI permite adicionar pastas extras por sala.
2. `discover_files` varre essas pastas recursivamente, filtra por extensão
   e faz uma checagem rápida do início do arquivo (`PokerRoom::sniff`) —
   confirmada contra hand history real só para PokerStars e GGPoker (mesmos
   marcadores do parser). PartyPoker/888poker/ACR usam uma heurística mais
   fraca hoje, documentada em `crates/scanner/src/room.rs`.
3. `SyncState` (um JSON por sala, em `app_config_dir()/sync-state/`) guarda
   tamanho+mtime de cada arquivo já sincronizado e quanto do texto já foi
   enviado — de um arquivo que cresceu (sessão rolando) só vai o trecho
   novo, a partir da última mão enviada (`scanner::text::new_part_since`).
4. Os arquivos são lidos em qualquer codificação comum (UTF-8, UTF-16,
   Windows-1252 — `scanner::text::decode_text`) e enviados em lotes que
   respeitam os limites do servidor (Vercel): até 50 arquivos e ~3 MB por
   envio; arquivo de mãos maior que 2 MB vai em partes, sempre cortadas no
   começo de uma mão (`sync_client::BatchBuilder`,
   `scanner::text::split_into_parts`). O progresso é salvo a cada lote
   aceito. O backend separa as mãos (`splitHands`) e deduplica por
   `external_hand_id`.
5. O corte do que importar ("só de agora em diante", "últimos 3 meses" ou
   "tudo") é aplicado no site. Quando o jogador AMPLIA a escolha, o Radar
   esquece o que já tinha enviado e manda de novo (`lembrar_escopo` em
   `src-tauri/src/lib.rs`) — senão o histórico que tinha ficado de fora
   nunca chegaria.

## Ciclo automático

`spawn_ciclo_automatico` (em `src-tauri/src/lib.rs`) roda logo ao abrir e
depois a cada 5 minutos: confere a sessão (renova se preciso), manda o
sinal de vida e, se estiver tudo liberado, envia mãos e torneios novos.
Sem internet (ex.: logo depois de ligar o PC), tenta de novo em 20 s,
40 s, 80 s… até o intervalo normal. Um envio por vez (`sync_lock`): o
automático e o "Sincronizar agora" nunca rodam juntos. A tela mostra a
situação (`status-changed`): tudo certo, sem internet, falta escolher o
que importar (a pergunta aparece ali mesmo, com os números do que o
Radar achou no computador), plano sem Radar ou sessão vencida.

## Autenticação

Dois caminhos, ambos contra o mesmo GoTrue do produto web:

- **Email/senha**: direto na janela do agente. A senha nunca é persistida.
- **Google**: o OAuth do Google não roda dentro da webview embutida do
  Tauri (Google bloqueia login em iframes/webviews embutidas por
  política de segurança) — o botão "Continuar com Google" abre o
  **navegador do sistema** numa página dedicada do produto
  (`gsimonetto/pokersync`, `app/agent-login/`), que faz o OAuth normal e
  devolve os tokens pro agente via deep link (`radar-pokersync://auth`,
  registrado pelo instalador — `tauri-plugin-deep-link`). Isso resolve o
  caso relatado de "logou pelo Google, a senha não pega aqui" — quem
  criou a conta assim nunca teve senha no Supabase pra começar.
  Um nonce (`state`) gerado antes de abrir o navegador e conferido na
  volta impede que um deep link de outra origem seja aceito como se
  fosse resposta desse login.

Em ambos os casos, só a chave de RENOVAÇÃO fica no keychain nativo do SO
(`keychain.rs`, via crate `keyring`: Windows Credential Manager, macOS
Keychain, Secret Service no Linux) — numa entrada só, nunca em disco em
texto plano. A chave de acesso (JWT de ~1 h) vive só na memória e é
renovada ao abrir. Se o Supabase recusar a chave de renovação, o Radar
limpa a sessão, avisa com notificação e mostra "Sua sessão expirou" na tela
de login (`sessao_expirou`); sem internet ele só tenta de novo depois,
sem deslogar. "Sair" encerra também a sessão deste Radar no servidor.

No Windows/Linux o link de volta do login com Google abre o programa de
novo — `tauri-plugin-single-instance` (com o recurso `deep-link`) repassa
o link pra cópia que já está rodando e fecha a segunda. O esquema fica em
`plugins.deep-link.desktop.schemes` no `tauri.conf.json` (fora de
`desktop`, o plugin ignora e nada é registrado).

## URL do PokerSync

O domínio de produção (`DEFAULT_BASE_URL` em `config.rs`) vem embutido no
binário — o jogador nunca vê nem precisa configurar isso. Só no build de
desenvolvimento (`cargo build`, nunca no instalador) dá pra apontar pra um
servidor de teste local: `RADAR_DEV_SITE_URL`, `RADAR_DEV_SUPABASE_URL`,
`RADAR_DEV_REFRESH_TOKEN` e `RADAR_DEV_INTERVAL_SECS`.

## Bandeja do sistema e início automático

Fechar a janela esconde em vez de encerrar o processo — o Radar é feito pra
ficar rodando em background. Clique no ícone perto do relógio abre a
janela; o menu (botão direito) tem "Abrir o Radar", "Sincronizar agora" e
"Fechar o Radar". "Abrir junto com o computador" (`tauri-plugin-autostart`)
liga sozinho no primeiro login e fica na tela principal pra desligar;
quando o SO abre o app no login, ele nasce só no ícone (flag `--hidden`).

## Rodando localmente

```bash
# testes dos crates puros (rápido, sem GUI)
cargo test -p scanner -p sync-client

# dev com hot-reload da UI (precisa de tauri-cli: cargo install tauri-cli --locked)
cargo tauri dev

# build de produção
cargo tauri build
```

Linux precisa de `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`,
`libayatana-appindicator3-dev`, `librsvg2-dev` instalados (ver docs do
Tauri v2 pra Windows/macOS).

## Identidade visual

`ui/` usa os mesmos tokens do produto web (`app/globals.css` em
`gsimonetto/pokersync`): fundo `--void` (#000), cards `--surface`/
`--elevated`, tipografia Space Grotesk, e a mesma paleta de acentos por
módulo (`--positive`/`--negative`/`--training`/`--evolution`/`--review`).
O logo (`pokersync-logo.svg`) é o arquivo real do produto, copiado pra cá.

**Badges das salas**: não são os logotipos oficiais de PokerStars/GGPoker/
PartyPoker/888poker/ACR — são iniciais num badge colorido, usando só as cores
do próprio design system do PokerSync (`ROOM_STYLE` em `ui/app.js`), não
as cores de marca de cada operadora. Decisão deliberada: fabricar de
memória um logotipo de terceiro é arriscado (fica errado, ou levanta
questão de uso de marca sem aprovação) — os badges atuais são um
placeholder honesto até alguém do time aprovar os assets oficiais de
cada sala pra substituir.

## O que falta (próximos passos)

- Trocar os badges de iniciais pelos logotipos oficiais de cada sala
  (precisa dos assets aprovados — ver "Identidade visual" acima).
- Fluxo de pareamento por código em vez de email/senha/Google.
- Validar `PokerRoom::default_search_paths` e `sniff` contra instalações
  reais de GGPoker, PartyPoker, 888poker e ACR (hoje só PokerStars e GGPoker
  têm parser validado no backend — ver `validateParsedHand` em
  `lib/poker/hand-parser.ts`; PartyPoker/888poker/ACR chegam como
  `raw_payload` com `parsed_data` best-effort até o parser ganhar suporte
  a esses formatos — `hand-parser.ts` hoje nem reconhece o texto de hand
  history dessas três salas, é o maior gap real da varredura hoje).
- Assinatura de código por SO (sem isso, Windows/macOS mostram aviso de
  "app não verificado" ao instalar). O ícone (cartas do logo PokerSync) já
  é o definitivo: `src-tauri/icons/`, com versão simplificada pros
  tamanhos pequenos.
- Avisar na hora em que o arquivo muda (hoje o ciclo automático confere a
  cada 5 minutos).
