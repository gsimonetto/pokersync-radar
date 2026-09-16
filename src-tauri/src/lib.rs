mod auth;
mod config;
mod keychain;

use config::AppConfig;
use keychain::Tokens;
use scanner::{discover_files, read_pending, FileKind, PokerRoom, SyncState};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use sync_client::{chunk_files, DeviceInfo, SyncClient, SyncFile, DEFAULT_BATCH_SIZE};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;

/// Intervalo do sync automático em background — curto o bastante pra uma
/// sessão de torneio aparecer no PokerSync pouco depois de terminar, sem
/// martelar disco/rede o dia inteiro parado numa mesa. Ver `spawn_auto_sync`.
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);

struct AppState {
    config_path: PathBuf,
    state_dir: PathBuf,
    config: Mutex<AppConfig>,
    /// Nonce do login com Google em andamento (gerado em
    /// `start_google_login`, conferido quando o deep link volta) —
    /// protege contra um deep link de origem estranha ser aceito como se
    /// fosse resposta de um login que o agente pediu.
    pending_google_state: Mutex<Option<String>>,
}

#[derive(serde::Serialize)]
struct ConfigDto {
    logged_in: bool,
    user_email: Option<String>,
    device_name: String,
    extra_folders: std::collections::HashMap<String, Vec<String>>,
    auto_sync_enabled: bool,
}

fn config_dto(c: &AppConfig) -> ConfigDto {
    ConfigDto {
        logged_in: keychain::load().is_some(),
        user_email: c.user_email.clone(),
        device_name: c.device_name.clone(),
        extra_folders: c.extra_folders.clone(),
        auto_sync_enabled: c.auto_sync_enabled,
    }
}

#[tauri::command]
fn get_config(state: State<AppState>) -> ConfigDto {
    config_dto(&state.config.lock().unwrap())
}

#[tauri::command]
fn save_device_name(state: State<AppState>, device_name: String) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.device_name = device_name;
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_extra_folders(
    state: State<AppState>,
    kind: String,
    folders: Vec<String>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.extra_folders.insert(kind, folders);
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_auto_sync_enabled(state: State<AppState>, enabled: bool) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.auto_sync_enabled = enabled;
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
async fn login(
    state: State<'_, AppState>,
    email: String,
    password: String,
) -> Result<ConfigDto, String> {
    let result = auth::login_with_password(&email, &password).await?;
    keychain::save(&Tokens {
        access_token: result.access_token,
        refresh_token: result.refresh_token,
    })?;
    let mut cfg = state.config.lock().unwrap();
    cfg.user_email = result.email;
    cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    Ok(config_dto(&cfg))
}

/// Abre o navegador do sistema na tela de login do agente (Google não
/// funciona dentro da webview embutida). O resultado volta assíncrono,
/// pelo deep link `radar-pokersync://auth` — ver `handle_deep_link`.
#[tauri::command]
fn start_google_login(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    use rand::Rng;
    let nonce: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    *state.pending_google_state.lock().unwrap() = Some(nonce.clone());

    let device_name = state.config.lock().unwrap().device_name.clone();
    let mut url = url::Url::parse(&format!("{}/agent-login", config::DEFAULT_BASE_URL))
        .expect("DEFAULT_BASE_URL é uma URL fixa e válida");
    url.query_pairs_mut()
        .append_pair("state", &nonce)
        .append_pair("device", &device_name);

    app.opener()
        .open_url(url.to_string(), None::<&str>)
        .map_err(|e| format!("Não consegui abrir o navegador: {e}"))
}

/// Extrai (code, state) de uma URL `radar-pokersync://auth?...` — usado
/// tanto pelo handler automático de deep link quanto pelo comando
/// `paste_login_link` (colar manual). `code` é um código de uso único
/// (ver `auth::exchange_login_code`), não mais o token de sessão em si.
fn parse_auth_deep_link(url: &url::Url) -> Option<(String, String)> {
    if url.scheme() != "radar-pokersync" || url.host_str() != Some("auth") {
        return None;
    }
    let params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let code = params.get("code")?.clone();
    let state = params.get("state").cloned().unwrap_or_default();
    Some((code, state))
}

/// Caminho manual pro login com Google: quando o SO não sabe abrir
/// `radar-pokersync://` sozinho (varia por SO/instalação — relatado como
/// "confirmo no navegador e fica só rodando"), a página de conclusão no
/// produto (app/agent-login/concluido) mostra esse link pra copiar. O
/// usuário cola aqui e o agente segue o mesmo caminho do deep link.
#[tauri::command]
async fn paste_login_link(app: AppHandle, link: String) -> Result<(), String> {
    let url = url::Url::parse(link.trim()).map_err(|_| "Link inválido — copie o link inteiro da página.".to_string())?;
    let (code, state) =
        parse_auth_deep_link(&url).ok_or("Esse link não é um link de login do Radar PokerSync.".to_string())?;
    complete_google_login(app, code, state).await;
    Ok(())
}

/// Chamado pelo handler de deep link (`run()`) quando
/// `radar-pokersync://auth?...` volta do login com Google. Confere o
/// nonce, troca o código de uso único pelos tokens reais
/// (`auth::exchange_login_code`) e salva a sessão — mesmo destino final
/// de `login()` (email/senha), só que assíncrono e sem senha nenhuma
/// passando pelo agente.
async fn complete_google_login(app: AppHandle, code: String, received_state: String) {
    let state = app.state::<AppState>();
    let state_matches = {
        let mut pending = state.pending_google_state.lock().unwrap();
        let matches = pending.as_deref() == Some(received_state.as_str()) && !received_state.is_empty();
        if matches {
            *pending = None;
        }
        matches
    };
    if !state_matches {
        let _ = app.emit(
            "google-login-result",
            serde_json::json!({ "ok": false, "error": "Login não corresponde ao que o agente pediu — tente de novo." }),
        );
        return;
    }

    let result = match auth::exchange_login_code(config::DEFAULT_BASE_URL, &code).await {
        Ok(r) => r,
        Err(e) => {
            let _ = app.emit("google-login-result", serde_json::json!({ "ok": false, "error": e }));
            return;
        }
    };

    if let Err(e) = keychain::save(&Tokens {
        access_token: result.access_token,
        refresh_token: result.refresh_token,
    }) {
        let _ = app.emit("google-login-result", serde_json::json!({ "ok": false, "error": e }));
        return;
    }

    {
        let mut cfg = state.config.lock().unwrap();
        cfg.user_email = result.email;
        let _ = cfg.save(&state.config_path);
    }

    let _ = app.emit("google-login-result", serde_json::json!({ "ok": true }));
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn logout(state: State<AppState>) -> Result<ConfigDto, String> {
    keychain::clear()?;
    let mut cfg = state.config.lock().unwrap();
    cfg.user_email = None;
    cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    Ok(config_dto(&cfg))
}

/// Sempre contra o domínio de produção (`config::DEFAULT_BASE_URL`) —
/// não existe mais campo de URL editável na UI, então não há "outro
/// site" pra testar.
#[tauri::command]
async fn test_connection(_state: State<'_, AppState>) -> Result<String, String> {
    let token = keychain::load()
        .ok_or("Faça login antes de testar a conexão.")?
        .access_token;
    let client = SyncClient::new(config::DEFAULT_BASE_URL, token);
    client.ping().await.map_err(|e| e.to_string())?;
    Ok("Conectado.".to_string())
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())
    } else {
        mgr.disable().map_err(|e| e.to_string())
    }
}

#[derive(serde::Serialize)]
struct RoomInfo {
    slug: String,
    display_name: String,
}

/// Só pra UI ter rótulo bonito por sala nos resultados (a tela não pede
/// mais "escolha a sala" — ver comentário em AppConfig::extra_folders).
#[tauri::command]
fn list_rooms() -> Vec<RoomInfo> {
    PokerRoom::ALL
        .into_iter()
        .map(|r| RoomInfo {
            slug: r.slug().to_string(),
            display_name: r.display_name().to_string(),
        })
        .collect()
}

fn parse_kind(slug: &str) -> Result<FileKind, String> {
    FileKind::from_slug(slug).ok_or_else(|| format!("Tipo de import desconhecido: {slug}"))
}

/// Nome do arquivo de estado por (sala, tipo) — mãos e torneios têm
/// progresso de sync independente, mesmo quando compartilham a mesma
/// sala/pasta.
fn state_file_name(room: PokerRoom, kind: FileKind) -> String {
    format!("{}-{}.json", room.slug(), kind.slug())
}

/// Pastas onde varrer esse tipo de arquivo, em TODAS as salas de uma vez —
/// os caminhos padrão de cada sala pro tipo pedido, mais as pastas extras
/// que o usuário escolheu manualmente (que não são mais por sala: uma
/// pasta manual é varrida contra todas as salas, o sniff decide de qual
/// sala cada arquivo é).
fn discover_all(state: &AppState, kind: FileKind) -> Vec<(PokerRoom, Vec<scanner::DiscoveredFile>)> {
    let extra: Vec<PathBuf> = state
        .config
        .lock()
        .unwrap()
        .extra_folders
        .get(kind.slug())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();

    PokerRoom::ALL
        .into_iter()
        .map(|room| {
            let mut roots = room.default_search_paths(kind);
            roots.extend(extra.iter().cloned());
            (room, discover_files(&roots, room, kind))
        })
        .collect()
}

#[derive(serde::Serialize)]
struct ScanSummary {
    room: String,
    files_found: usize,
    files_pending: usize,
}

/// Só varre e conta — não sincroniza nada. É o que a UI mostra antes do
/// usuário confirmar "Verificar agora" (ou o que o sync automático usa
/// internamente pra saber se vale a pena sincronizar).
#[tauri::command]
fn scan_preview(state: State<AppState>, kind: String) -> Result<Vec<ScanSummary>, String> {
    let kind = parse_kind(&kind)?;
    let mut out = Vec::new();
    for (room, found) in discover_all(&state, kind) {
        let sync_state = SyncState::load(&state.state_dir.join(state_file_name(room, kind)));
        let pending = read_pending(&found, &sync_state).len();
        out.push(ScanSummary {
            room: room.slug().to_string(),
            files_found: found.len(),
            files_pending: pending,
        });
    }
    Ok(out)
}

#[derive(serde::Serialize, Default)]
struct SyncSummary {
    room: String,
    files_synced: usize,
    total_items: u32,
    imported: u32,
    duplicates: u32,
    errors: u32,
}

async fn refresh_client() -> Result<SyncClient, String> {
    let refresh_token = keychain::load()
        .ok_or("Sessão expirada — faça login novamente.")?
        .refresh_token;
    let result = auth::refresh_session(&refresh_token).await?;
    keychain::save(&Tokens {
        access_token: result.access_token.clone(),
        refresh_token: result.refresh_token,
    })?;
    Ok(SyncClient::new(config::DEFAULT_BASE_URL, result.access_token))
}

/// Sincroniza um tipo de arquivo (mãos ou torneios) em todas as salas.
/// Usado tanto pelo comando `sync_now` (clique manual) quanto pelo laço de
/// sync automático em background (`spawn_auto_sync`) — mesma lógica,
/// evita os dois caminhos divergirem.
async fn sync_kind(state: &AppState, kind: FileKind) -> Result<Vec<SyncSummary>, String> {
    let (token, device) = {
        let cfg = state.config.lock().unwrap();
        let token = keychain::load()
            .ok_or("Faça login antes de sincronizar.")?
            .access_token;
        let device = DeviceInfo {
            device_id: cfg.device_id.clone(),
            device_name: cfg.device_name.clone(),
            platform: std::env::consts::OS.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        (token, device)
    };
    let mut client = SyncClient::new(config::DEFAULT_BASE_URL, token);
    // Sessão do agente pode ter expirado desde o login — renova uma vez com
    // o refresh_token e recria o client, em vez de forçar login manual de
    // novo a cada sync.
    let mut already_refreshed = false;

    let mut out = Vec::new();
    for (room, found) in discover_all(state, kind) {
        let state_path = state.state_dir.join(state_file_name(room, kind));
        let mut sync_state = SyncState::load(&state_path);
        let pending = read_pending(&found, &sync_state);

        if pending.is_empty() {
            out.push(SyncSummary {
                room: room.slug().to_string(),
                ..Default::default()
            });
            continue;
        }

        let files: Vec<SyncFile> = pending
            .iter()
            .map(|p| SyncFile {
                raw_text: p.content.clone(),
                captured_at: None,
            })
            .collect();

        let mut total_items = 0u32;
        let mut imported = 0u32;
        let mut duplicates = 0u32;
        let mut errors = 0u32;

        for (batch_files, batch_pending) in chunk_files(files, DEFAULT_BATCH_SIZE)
            .into_iter()
            .zip(pending.chunks(DEFAULT_BATCH_SIZE))
        {
            let (total, imp, dup, err) = match kind {
                FileKind::HandHistory => {
                    let mut attempt = client.sync_batch(&device, room.slug(), &batch_files).await;
                    if let Err(sync_client::SyncError::Rejected { status: 401, .. }) = &attempt {
                        if !already_refreshed {
                            already_refreshed = true;
                            client = refresh_client().await?;
                            attempt = client.sync_batch(&device, room.slug(), &batch_files).await;
                        }
                    }
                    let r = attempt.map_err(|e| e.to_string())?;
                    (r.total_hands, r.imported, r.duplicates, r.errors)
                }
                FileKind::TournamentSummary => {
                    let mut attempt = client.sync_tournament_batch(&device, room.slug(), &batch_files).await;
                    if let Err(sync_client::SyncError::Rejected { status: 401, .. }) = &attempt {
                        if !already_refreshed {
                            already_refreshed = true;
                            client = refresh_client().await?;
                            attempt = client.sync_tournament_batch(&device, room.slug(), &batch_files).await;
                        }
                    }
                    let r = attempt.map_err(|e| e.to_string())?;
                    (r.total_files, r.imported, r.duplicates, r.errors)
                }
            };
            total_items += total;
            imported += imp;
            duplicates += dup;
            errors += err;
            for p in batch_pending {
                sync_state.mark_synced(p.path.clone(), p.signature);
            }
        }

        sync_state.save(&state_path).map_err(|e| e.to_string())?;
        out.push(SyncSummary {
            room: room.slug().to_string(),
            files_synced: pending.len(),
            total_items,
            imported,
            duplicates,
            errors,
        });
    }
    Ok(out)
}

#[tauri::command]
async fn sync_now(state: State<'_, AppState>, kind: String) -> Result<Vec<SyncSummary>, String> {
    sync_kind(&state, parse_kind(&kind)?).await
}

/// Sync automático em background: dispara mãos + torneios a cada
/// `AUTO_SYNC_INTERVAL`, sem depender do jogador clicar em nada. Falha
/// silenciosamente (não logado, sem rede) — não é
/// pra encher a tela de erro por um laço que roda sozinho; erros reais
/// ainda aparecem quando o jogador abre a janela e vê "há X sem
/// sincronizar" nunca mudar. Emite `auto-sync-result` só quando dá certo
/// e importa algo de fato, pra UI mostrar sem virar barulho.
fn spawn_auto_sync(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(AUTO_SYNC_INTERVAL);
        interval.tick().await; // primeiro tick é imediato; espera 1 ciclo antes do primeiro sync
        loop {
            interval.tick().await;
            let state = app.state::<AppState>();
            let enabled = state.config.lock().unwrap().auto_sync_enabled;
            if !enabled || keychain::load().is_none() {
                continue;
            }
            let mut imported_total = 0u32;
            let mut synced_any = false;
            for kind in [FileKind::HandHistory, FileKind::TournamentSummary] {
                if let Ok(summaries) = sync_kind(&state, kind).await {
                    for s in &summaries {
                        imported_total += s.imported;
                        if s.files_synced > 0 {
                            synced_any = true;
                        }
                    }
                }
            }
            if synced_any {
                let _ = app.emit(
                    "auto-sync-result",
                    serde_json::json!({ "imported": imported_total, "at": now_epoch_secs() }),
                );
            }
        }
    });
}

fn now_epoch_secs() -> String {
    // Sem dependência extra só pra isso: formato ISO 8601 simples via
    // SystemTime, suficiente pra UI mostrar "última sincronização".
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // Argumento passado quando o SO abre o app sozinho no login —
            // usado no setup() abaixo pra abrir minimizado na bandeja em
            // vez de estourar a janela na cara do usuário todo boot.
            Some(vec!["--hidden"]),
        ))
        .setup(|app| {
            let config_dir = app.path().app_config_dir().expect("sem app_config_dir");
            let config_path = config_dir.join("config.json");
            let state_dir = config_dir.join("sync-state");
            let config = AppConfig::load(&config_path);
            app.manage(AppState {
                config_path,
                state_dir,
                config: Mutex::new(config),
                pending_google_state: Mutex::new(None),
            });

            // Sync automático em background — roda o dia inteiro sozinho,
            // sem o jogador precisar abrir a janela e clicar em nada.
            spawn_auto_sync(app.handle().clone());

            // Login com Google: radar-pokersync://auth?code=...&state=...
            // volta aqui depois do navegador do sistema completar o OAuth
            // (ver start_google_login e app/agent-login no produto). Só
            // funciona quando o SO sabe abrir o esquema customizado — nem
            // sempre acontece sozinho (varia por SO/forma de instalação:
            // instalador que não rodou com permissão de escrever no
            // registro, versão antiga que registrou o esquema errado
            // antes do rename, AppImage sem integração no Linux etc.).
            // `register_all()` reforça esse registro toda vez que o app
            // abre — reescreve a chave do Windows apontando pro executável
            // atual, sem precisar de reinstalação. Continua existindo
            // também o comando `paste_login_link` como caminho manual pra
            // quando mesmo assim não funcionar (mesmo parser, ver
            // `parse_auth_deep_link`).
            let _ = app.deep_link().register_all();

            let deep_link_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let Some((code, received_state)) = parse_auth_deep_link(&url) else {
                        continue;
                    };
                    let handle = deep_link_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        complete_google_login(handle, code, received_state).await;
                    });
                }
            });

            let show_i = MenuItem::with_id(app, "show", "Mostrar", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Sair", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&tray_menu)
                .show_menu_on_left_click(true)
                .tooltip("Radar PokerSync")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Iniciado pelo SO no login (--hidden): fica só na bandeja,
            // sem abrir a janela.
            let launched_hidden = std::env::args().any(|a| a == "--hidden");
            if launched_hidden {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            Ok(())
        })
        // Fechar a janela (X) minimiza pra bandeja em vez de encerrar o
        // processo — o agente é feito pra ficar rodando em background.
        // Sair de verdade é só pelo menu da bandeja ("Sair").
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_device_name,
            save_extra_folders,
            set_auto_sync_enabled,
            login,
            start_google_login,
            paste_login_link,
            logout,
            test_connection,
            list_rooms,
            scan_preview,
            sync_now,
            get_autostart,
            set_autostart,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao rodar o app Tauri");
}
