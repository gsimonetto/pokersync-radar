mod auth;
mod config;
mod importar;
mod keychain;
mod watcher;

use config::AppConfig;
use scanner::text::{fingerprint, new_part_since, split_into_parts};
use scanner::state::now_unix;
use scanner::{
    discover, pending_files, read_text, search_roots, ClassCache, DiscoveredFile, FileKind, FileSignature,
    PokerRoom, SentText, SyncState,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use sync_client::{
    BatchBuilder, DeviceInfo, SyncClient, SyncError, SyncFile, MAX_BATCH_BYTES, MAX_FILES_PER_BATCH,
    MAX_HAND_PART_BYTES, MAX_SUMMARY_BYTES,
};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

/// Intervalo do ciclo automático — curto o bastante pra uma sessão de
/// torneio aparecer no PokerSync pouco depois de terminar, sem martelar
/// disco/rede o dia inteiro parado numa mesa. Ver `spawn_ciclo_automatico`.
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Primeira espera depois de uma falha de rede — dobra a cada falha
/// seguida, até o intervalo normal. No boot do Windows a internet costuma
/// subir alguns segundos depois do Radar.
const RETRY_INICIAL: Duration = Duration::from_secs(20);
/// Renova a chave de acesso quando faltar menos que isso pra ela vencer.
const MARGEM_RENOVACAO_SEGS: i64 = 120;
/// Quantas vezes, no mesmo envio, o Radar espera o tempo que o site pediu
/// (limite de envios por minuto) antes de deixar pro próximo ciclo.
const MAX_ESPERAS_LIMITE: u32 = 3;
const TRAY_ID: &str = "radar";

/// Sessão com o PokerSync. A chave de acesso (JWT, vence em ~1 hora) só
/// vive na memória; no cofre do sistema fica só a chave de renovação (ver
/// keychain.rs). Logo depois de abrir o app a chave de acesso é `None` e o
/// primeiro ciclo renova.
struct Session {
    access_token: Option<String>,
    refresh_token: String,
}

/// O que a tela (e a dica do ícone perto do relógio) mostra sobre o Radar.
#[derive(Clone, serde::Serialize)]
struct RadarStatus {
    /// "desconectado" | "conectando" | "ok" | "sem_internet" | "sessao_expirada"
    conexao: &'static str,
    sincronizando: bool,
    /// Resposta do site: `None` = o jogador ainda não escolheu o que importar.
    import_scope: Option<String>,
    /// Resposta do site: `Some(false)` = o plano da conta não inclui o Radar.
    radar_liberado: Option<bool>,
    /// Fim do último ciclo que terminou sem erro (segundos desde 1970).
    ultima_sincronizacao: Option<u64>,
    /// Quantos itens novos entraram no último ciclo que trouxe novidade, e quando.
    ultimas_novidades: Option<u32>,
    ultimas_novidades_em: Option<u64>,
    ultimo_erro: Option<String>,
    /// Arquivos que o site recusou e o Radar deixou de lado (tenta de novo
    /// em um dia, ou quando o arquivo mudar) — o resto segue sendo enviado.
    arquivos_recusados: u32,
    /// Pastas vigiadas em tempo real (ver watcher.rs). Zero = só o ciclo
    /// de 5 minutos (nenhuma pasta de sala existe ainda neste computador).
    pastas_vigiadas: u32,
}

impl RadarStatus {
    fn inicial(logado: bool, sessao_expirada: bool) -> Self {
        RadarStatus {
            conexao: if logado {
                "conectando"
            } else if sessao_expirada {
                "sessao_expirada"
            } else {
                "desconectado"
            },
            sincronizando: false,
            import_scope: None,
            radar_liberado: None,
            ultima_sincronizacao: None,
            ultimas_novidades: None,
            ultimas_novidades_em: None,
            ultimo_erro: None,
            arquivos_recusados: 0,
            pastas_vigiadas: 0,
        }
    }
}

struct AppState {
    config_path: PathBuf,
    state_dir: PathBuf,
    config: Mutex<AppConfig>,
    /// Nonce do login com Google em andamento (gerado em
    /// `start_google_login`, conferido quando o deep link volta) —
    /// protege contra um deep link de origem estranha ser aceito como se
    /// fosse resposta de um login que o agente pediu.
    pending_google_state: Mutex<Option<String>>,
    session: tokio::sync::Mutex<Option<Session>>,
    logado: AtomicBool,
    /// Um envio por vez: o ciclo automático e o "Sincronizar agora" nunca
    /// rodam juntos (evita mandar o mesmo arquivo duas vezes).
    sync_lock: tokio::sync::Mutex<()>,
    /// Uma varredura de disco por vez (o resumo da tela inicial e o envio
    /// podem pedir ao mesmo tempo, e os dois gravam a memória de
    /// reconhecimento — ver `discover_all`).
    discover_lock: Mutex<()>,
    /// Vigilância das pastas em tempo real e o canal por onde ela avisa
    /// (ver `spawn_vigia`).
    vigia: Mutex<watcher::Vigia>,
    aviso_vigia: tokio::sync::mpsc::UnboundedSender<()>,
    status: Mutex<RadarStatus>,
    /// Acorda o ciclo automático na hora (depois de um login, por exemplo)
    /// em vez de esperar os 5 minutos.
    wake: tokio::sync::Notify,
}

fn agora() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn texto_bandeja(s: &RadarStatus) -> String {
    let detalhe = match s.conexao {
        "sessao_expirada" | "desconectado" => "entre na sua conta",
        "sem_internet" => "sem conexão, tentando de novo",
        "conectando" => "conectando…",
        _ if s.radar_liberado == Some(false) => "seu plano não inclui o Radar",
        _ if s.import_scope.is_none() => "falta escolher no site o que importar",
        _ if s.sincronizando => "sincronizando…",
        _ if s.pastas_vigiadas > 0 => "sincronizando em tempo real",
        _ => "sincronizando sozinho",
    };
    format!("Radar PokerSync — {detalhe}")
}

/// Muda o status, avisa a tela e atualiza a dica do ícone perto do relógio.
fn atualizar_status(app: &AppHandle, f: impl FnOnce(&mut RadarStatus)) {
    let novo = {
        let state = app.state::<AppState>();
        let mut s = state.status.lock().unwrap();
        f(&mut s);
        s.clone()
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(texto_bandeja(&novo)));
    }
    let _ = app.emit("status-changed", &novo);
}

fn mostrar_janela(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn device_info(state: &AppState) -> DeviceInfo {
    let cfg = state.config.lock().unwrap();
    DeviceInfo {
        device_id: cfg.device_id.clone(),
        device_name: cfg.device_name.clone(),
        platform: std::env::consts::OS.to_string(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[derive(serde::Serialize)]
struct ConfigDto {
    logged_in: bool,
    user_email: Option<String>,
    device_name: String,
    extra_folders: HashMap<String, Vec<String>>,
    auto_sync_enabled: bool,
    sessao_expirada: bool,
}

fn config_dto(c: &AppConfig, logged_in: bool) -> ConfigDto {
    ConfigDto {
        logged_in,
        user_email: c.user_email.clone(),
        device_name: c.device_name.clone(),
        extra_folders: c.extra_folders.clone(),
        auto_sync_enabled: c.auto_sync_enabled,
        sessao_expirada: c.sessao_expirada,
    }
}

// ---------------------------------------------------------------------------
// Sessão: renovação, expiração e chamadas ao site
// ---------------------------------------------------------------------------

enum AuthError {
    NaoLogado,
    Expirada,
    SemInternet,
}

impl AuthError {
    fn mensagem(&self) -> String {
        match self {
            AuthError::NaoLogado => "Entre na sua conta PokerSync pra sincronizar.",
            AuthError::Expirada => "Sua sessão expirou — entre de novo.",
            AuthError::SemInternet => {
                "Sem conexão com o PokerSync — confira a internet. O Radar tenta de novo sozinho."
            }
        }
        .to_string()
    }
}

enum Falha {
    Auth(AuthError),
    Site(SyncError),
}

impl Falha {
    fn mensagem(&self) -> String {
        match self {
            Falha::Auth(a) => a.mensagem(),
            Falha::Site(e) => e.to_string(),
        }
    }
}

/// Chave de acesso pronta pra usar. Usa a da memória enquanto ela valer
/// por mais uns minutos; senão renova (o lock da sessão garante uma
/// renovação por vez). `falhou` = chave que o site acabou de recusar com
/// 401: se outra tarefa já renovou nesse meio-tempo, só devolve a nova.
async fn chave_de_acesso(app: &AppHandle, falhou: Option<&str>) -> Result<String, AuthError> {
    let state = app.state::<AppState>();
    let mut guard = state.session.lock().await;
    let Some(session) = guard.as_mut() else {
        return Err(AuthError::NaoLogado);
    };
    if let Some(atual) = &session.access_token {
        let serve = match falhou {
            Some(recusada) => atual != recusada,
            None => auth::seconds_until_expiry(atual).is_some_and(|s| s > MARGEM_RENOVACAO_SEGS),
        };
        if serve {
            return Ok(atual.clone());
        }
    }
    match auth::refresh_session(&session.refresh_token).await {
        Ok(r) => {
            // Falhar ao gravar no cofre não derruba a sessão atual (ela
            // segue na memória); só não sobreviveria a um reinício.
            if let Err(e) = keychain::save_refresh_token(&r.refresh_token) {
                eprintln!("[radar] não consegui gravar a sessão no cofre do sistema: {e}");
            }
            session.refresh_token = r.refresh_token;
            session.access_token = Some(r.access_token.clone());
            Ok(r.access_token)
        }
        Err(auth::RefreshError::Invalid) => {
            *guard = None;
            drop(guard);
            sessao_expirou(app);
            Err(AuthError::Expirada)
        }
        Err(auth::RefreshError::Transient(e)) => {
            eprintln!("[radar] renovação da sessão falhou (vai tentar de novo): {e}");
            Err(AuthError::SemInternet)
        }
    }
}

/// A sessão acabou de verdade (o Supabase recusou a chave de renovação):
/// limpa tudo, deixa o motivo gravado pra tela de login e avisa com uma
/// notificação — antes o Radar continuava mostrando "logado" e falhando
/// calado a cada 5 minutos.
fn sessao_expirou(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.logado.store(false, Ordering::SeqCst);
    let _ = keychain::clear();
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.sessao_expirada = true;
        let _ = cfg.save(&state.config_path);
    }
    atualizar_status(app, |s| {
        s.conexao = "sessao_expirada";
        s.sincronizando = false;
        s.ultimo_erro = None;
    });
    let _ = app
        .notification()
        .builder()
        .title("Radar PokerSync")
        .body("Sua sessão expirou. Abra o Radar e entre de novo pra continuar sincronizando suas mãos.")
        .show();
}

/// Chamada ao site com a chave atual; se o site responder 401, renova a
/// sessão uma vez e tenta de novo.
async fn chamar_site<T, F, Fut>(app: &AppHandle, chamada: F) -> Result<T, Falha>
where
    F: Fn(SyncClient) -> Fut,
    Fut: Future<Output = Result<T, SyncError>>,
{
    let chave = chave_de_acesso(app, None).await.map_err(Falha::Auth)?;
    match chamada(SyncClient::new(config::site_url(), chave.clone())).await {
        Err(e) if e.status() == Some(401) => {
            let nova = chave_de_acesso(app, Some(&chave)).await.map_err(Falha::Auth)?;
            chamada(SyncClient::new(config::site_url(), nova))
                .await
                .map_err(Falha::Site)
        }
        outro => outro.map_err(Falha::Site),
    }
}

/// Liga a sessão depois de um login (senha ou Google). No primeiro login
/// neste computador também liga o "iniciar com o computador" — dá pra
/// desligar na tela principal.
async fn iniciar_sessao(app: &AppHandle, r: auth::LoginResult) -> ConfigDto {
    if let Err(e) = keychain::save_refresh_token(&r.refresh_token) {
        eprintln!("[radar] não consegui gravar a sessão no cofre do sistema: {e}");
    }
    let state = app.state::<AppState>();
    *state.session.lock().await = Some(Session {
        access_token: Some(r.access_token),
        refresh_token: r.refresh_token,
    });
    state.logado.store(true, Ordering::SeqCst);
    let dto = {
        let mut cfg = state.config.lock().unwrap();
        cfg.user_email = r.email;
        cfg.sessao_expirada = false;
        if !cfg.autostart_configurado && app.autolaunch().enable().is_ok() {
            cfg.autostart_configurado = true;
        }
        let _ = cfg.save(&state.config_path);
        config_dto(&cfg, true)
    };
    atualizar_status(app, |s| {
        *s = RadarStatus::inicial(true, false);
    });
    state.wake.notify_one();
    dto
}

// ---------------------------------------------------------------------------
// Comandos chamados pela tela
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_config(state: State<AppState>) -> ConfigDto {
    let cfg = state.config.lock().unwrap();
    config_dto(&cfg, state.logado.load(Ordering::SeqCst))
}

#[tauri::command]
fn get_status(state: State<AppState>) -> RadarStatus {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn save_device_name(state: State<AppState>, device_name: String) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.device_name = device_name;
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_extra_folders(
    app: AppHandle,
    state: State<AppState>,
    kind: String,
    folders: Vec<String>,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.extra_folders.insert(kind, folders);
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    // Pasta nova já passa a ser vigiada na hora.
    atualizar_vigia(&app);
    Ok(())
}

#[tauri::command]
fn set_auto_sync_enabled(state: State<AppState>, enabled: bool) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.auto_sync_enabled = enabled;
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    if enabled {
        state.wake.notify_one();
    }
    Ok(())
}

#[tauri::command]
async fn login(app: AppHandle, email: String, password: String) -> Result<ConfigDto, String> {
    let result = auth::login_with_password(email.trim(), &password).await?;
    Ok(iniciar_sessao(&app, result).await)
}

/// Abre o navegador do sistema na tela de login do agente (Google não
/// funciona dentro da webview embutida). O resultado volta assíncrono,
/// pelo deep link `radar-pokersync://auth` — ver `complete_google_login`.
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
    let mut url = url::Url::parse(&format!("{}/agent-login", config::site_url()))
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
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let code = params.get("code")?.clone();
    let state = params.get("state").cloned().unwrap_or_default();
    Some((code, state))
}

/// Caminho manual pro login com Google: quando o SO não abre
/// `radar-pokersync://` sozinho, a página de conclusão no produto
/// (app/agent-login/concluido) mostra esse link pra copiar. O usuário cola
/// aqui e o agente segue o mesmo caminho do deep link.
#[tauri::command]
async fn paste_login_link(app: AppHandle, link: String) -> Result<(), String> {
    let url = url::Url::parse(link.trim()).map_err(|_| "Link inválido — copie o link inteiro da página.".to_string())?;
    let (code, state) =
        parse_auth_deep_link(&url).ok_or("Esse link não é um link de login do Radar PokerSync.".to_string())?;
    complete_google_login(app, code, state).await;
    Ok(())
}

/// Chamado quando `radar-pokersync://auth?...` volta do login com Google
/// (ou é colado à mão). Confere o nonce, troca o código de uso único pela
/// sessão (`auth::exchange_login_code`) e liga a sessão — mesmo destino do
/// login por senha.
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
            serde_json::json!({ "ok": false, "error": "Login não corresponde ao que o Radar pediu — clique em \"Continuar com Google\" de novo." }),
        );
        return;
    }

    match auth::exchange_login_code(config::site_url(), &code).await {
        Ok(result) => {
            iniciar_sessao(&app, result).await;
            let _ = app.emit("google-login-result", serde_json::json!({ "ok": true }));
            mostrar_janela(&app);
        }
        Err(e) => {
            let _ = app.emit("google-login-result", serde_json::json!({ "ok": false, "error": e }));
        }
    }
}

/// Sai da conta neste computador e encerra a sessão deste Radar no
/// servidor também (antes ela continuava valendo lá).
#[tauri::command]
async fn logout(app: AppHandle) -> Result<ConfigDto, String> {
    let state = app.state::<AppState>();
    let sessao = state.session.lock().await.take();
    state.logado.store(false, Ordering::SeqCst);
    if let Some(s) = sessao {
        let chave = match s
            .access_token
            .filter(|t| auth::seconds_until_expiry(t).is_some_and(|left| left > 30))
        {
            Some(t) => Some(t),
            None => auth::refresh_session(&s.refresh_token).await.ok().map(|r| r.access_token),
        };
        if let Some(t) = chave {
            auth::logout_remote(&t).await;
        }
    }
    let _ = keychain::clear();
    let dto = {
        let mut cfg = state.config.lock().unwrap();
        cfg.user_email = None;
        cfg.sessao_expirada = false;
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
        config_dto(&cfg, false)
    };
    atualizar_status(&app, |s| *s = RadarStatus::inicial(false, false));
    Ok(dto)
}

/// Sempre contra o domínio de produção (`config::site_url()`).
#[tauri::command]
async fn test_connection(app: AppHandle) -> Result<String, String> {
    let device = device_info(&app.state::<AppState>());
    let d = &device;
    match chamar_site(&app, |c| async move { c.ping(Some(d)).await }).await {
        Ok(info) => {
            atualizar_status(&app, |s| {
                s.conexao = "ok";
                s.import_scope = info.import_scope.clone();
                s.radar_liberado = info.radar_liberado;
                s.ultimo_erro = None;
            });
            Ok(match info.email {
                Some(email) => format!("Conectado como {email}."),
                None => "Conectado.".to_string(),
            })
        }
        Err(f) => {
            let mensagem = f.mensagem();
            registrar_falha(&app, f);
            Err(mensagem)
        }
    }
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, state: State<AppState>, enabled: bool) -> Result<(), String> {
    let mgr = app.autolaunch();
    if enabled { mgr.enable() } else { mgr.disable() }.map_err(|e| e.to_string())?;
    // A partir daqui vale a escolha do jogador — o Radar não liga sozinho de novo.
    let mut cfg = state.config.lock().unwrap();
    cfg.autostart_configurado = true;
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

/// Abre uma página do PokerSync no navegador — só destinos conhecidos.
#[tauri::command]
fn abrir_no_site(app: AppHandle, destino: String) -> Result<(), String> {
    let caminho = match destino.as_str() {
        "escolher-importacao" => "/radar",
        "planos" => "/planos",
        _ => "/",
    };
    app.opener()
        .open_url(format!("{}{caminho}", config::site_url()), None::<&str>)
        .map_err(|e| format!("Não consegui abrir o navegador: {e}"))
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

/// Sala cujos arquivos o site já sabe ler. O ACR ficou segurado aqui até o
/// site aprender o formato (`lib/poker/hand-parser.ts` e o resumo .ots em
/// `lib/poker/tournament-summary-parser.ts`, em gsimonetto/pokersync,
/// validados com arquivos reais em 03/10/2026) — liberado. Como nada do ACR
/// tinha sido marcado como enviado, o histórico inteiro vai na hora. Fica a
/// função pra próxima sala que o site ainda não souber ler.
fn envio_liberado(_room: PokerRoom) -> bool {
    true
}

/// Nome do arquivo de estado por (sala, tipo) — mãos e torneios têm
/// progresso de sync independente, mesmo quando compartilham a mesma
/// sala/pasta.
fn state_file_name(room: PokerRoom, kind: FileKind) -> String {
    format!("{}-{}.json", room.slug(), kind.slug())
}

/// Pastas que o jogador adicionou pra esse tipo de arquivo.
fn pastas_extras(state: &AppState, kind: FileKind) -> Vec<PathBuf> {
    state
        .config
        .lock()
        .unwrap()
        .extra_folders
        .get(kind.slug())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Passa a vigiar as pastas de hoje (padrão de cada sala que existem no
/// disco + as adicionadas) — chamado ao abrir, ao mudar as pastas e a cada
/// ciclo (uma sala instalada depois começa a ser vigiada sozinha).
fn atualizar_vigia(app: &AppHandle) {
    let state = app.state::<AppState>();
    let candidatas: Vec<PathBuf> = [FileKind::HandHistory, FileKind::TournamentSummary]
        .into_iter()
        .flat_map(|kind| search_roots(kind, &pastas_extras(&state, kind)))
        .map(|r| r.path)
        .collect();
    let pastas = watcher::pastas_para_vigiar(candidatas);
    let vigiadas = state.vigia.lock().unwrap().atualizar(pastas, &state.aviso_vigia) as u32;
    if state.status.lock().unwrap().pastas_vigiadas != vigiadas {
        atualizar_status(app, |s| s.pastas_vigiadas = vigiadas);
    }
}

/// Recebe os avisos da vigilância e, `watcher::ESPERA_APOS_MUDANCA` depois
/// do primeiro, acorda o ciclo automático (juntando os avisos que chegaram
/// nesse meio-tempo num envio só). Com a sincronização automática
/// desligada ou fora da conta, os avisos são ignorados.
fn spawn_vigia(app: AppHandle, mut avisos: tokio::sync::mpsc::UnboundedReceiver<()>) {
    tauri::async_runtime::spawn(async move {
        while avisos.recv().await.is_some() {
            tokio::time::sleep(watcher::ESPERA_APOS_MUDANCA).await;
            while avisos.try_recv().is_ok() {}
            let state = app.state::<AppState>();
            let ligado = state.config.lock().unwrap().auto_sync_enabled;
            if ligado && state.logado.load(Ordering::SeqCst) {
                state.wake.notify_one();
            }
        }
    });
}

/// Arquivos desse tipo em TODAS as salas de uma vez, separados por sala —
/// nas pastas padrão de cada sala e nas pastas extras que o jogador
/// escolheu (ver `scanner::discover`: cada pasta é percorrida uma vez e
/// cada arquivo fica com uma sala só). A memória de reconhecimento fica ao
/// lado do progresso de envio e evita reabrir, a cada 5 minutos, arquivos
/// que não mudaram.
fn discover_all(state: &AppState, kind: FileKind) -> Vec<(PokerRoom, Vec<DiscoveredFile>)> {
    let extra = pastas_extras(state, kind);

    let found = {
        let _uma_por_vez = state.discover_lock.lock().unwrap();
        let cache_path = state.state_dir.join(format!("reconhecidos-{}.json", kind.slug()));
        let mut cache = ClassCache::load(&cache_path);
        let found = discover(&search_roots(kind, &extra), kind, &mut cache);
        if let Err(e) = cache.save_if_changed(&cache_path) {
            eprintln!("[radar] não consegui salvar a memória de reconhecimento: {e}");
        }
        found
    };

    let mut por_sala: Vec<(PokerRoom, Vec<DiscoveredFile>)> =
        PokerRoom::ALL.into_iter().map(|room| (room, Vec::new())).collect();
    for f in found {
        if let Some((_, lista)) = por_sala.iter_mut().find(|(room, _)| *room == f.room) {
            lista.push(f);
        }
    }
    por_sala
}

#[derive(serde::Serialize)]
struct ScanSummary {
    room: String,
    files_found: usize,
    files_pending: usize,
}

/// Só varre e conta — não lê o conteúdo nem sincroniza nada.
#[tauri::command]
fn scan_preview(state: State<AppState>, kind: String) -> Result<Vec<ScanSummary>, String> {
    let kind = parse_kind(&kind)?;
    let mut out = Vec::new();
    for (room, found) in discover_all(&state, kind) {
        let sync_state = SyncState::load(&state.state_dir.join(state_file_name(room, kind)));
        let pending = pending_files(&found, &sync_state).len();
        out.push(ScanSummary {
            room: room.slug().to_string(),
            files_found: found.len(),
            files_pending: pending,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Envio de arquivos
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Default)]
struct SyncSummary {
    room: String,
    files_synced: usize,
    total_items: u32,
    imported: u32,
    duplicates: u32,
    errors: u32,
    ignored_by_date: u32,
    /// Arquivos que o site recusou e ficaram de lado (ver `SyncState::record_failure`).
    files_rejected: u32,
    /// Arquivos achados de uma sala que o site ainda não sabe ler (ver
    /// `envio_liberado`) — ficam esperando, sem ser enviados.
    files_waiting: u32,
}

/// Arquivo cujas partes já estão todas no lote atual — só vira "enviado"
/// quando o site aceitar o lote.
struct Concluido {
    path: PathBuf,
    signature: FileSignature,
    enviado: SentText,
}

/// Marca como enviados os arquivos concluídos (menos os que o site recusou
/// e ficaram de lado) e devolve quantos foram.
fn marcar_enviados(
    sync_state: &mut SyncState,
    state_path: &Path,
    concluidos: &mut Vec<Concluido>,
    recusados: &HashSet<PathBuf>,
) -> usize {
    if concluidos.is_empty() {
        return 0;
    }
    let mut marcados = 0;
    for c in concluidos.drain(..) {
        if recusados.contains(&c.path) {
            continue;
        }
        sync_state.mark_synced_text(c.path, c.signature, c.enviado);
        marcados += 1;
    }
    // Progresso salvo a cada lote aceito: se o próximo lote falhar, o que
    // já foi não é reenviado.
    salvar_progresso(sync_state, state_path);
    marcados
}

fn salvar_progresso(sync_state: &SyncState, state_path: &Path) {
    if let Err(e) = sync_state.save(state_path) {
        eprintln!("[radar] não consegui salvar o progresso do envio: {e}");
    }
}

/// Recusa do site que vale investigar arquivo por arquivo, em vez de parar
/// o envio inteiro: `Some(true)` = recusa pelo conteúdo (corpo inválido,
/// grande demais — tentar de novo não muda nada), `Some(false)` = erro ao
/// processar no servidor (pode ser o arquivo, pode ser passageiro). `None`
/// = problema que não é do arquivo (sessão, plano, escolha do que
/// importar, limite de envios, site fora do ar, sem internet): para tudo
/// e tenta no próximo ciclo, como antes.
fn recusa_por_arquivo(falha: &Falha) -> Option<bool> {
    let Falha::Site(SyncError::Rejected { status, code, .. }) = falha else {
        return None;
    };
    if matches!(code.as_deref(), Some("IMPORT_SCOPE_NAO_DEFINIDO" | "RADAR_FORA_DO_PLANO")) {
        return None;
    }
    match status {
        400 | 413 | 415 | 422 => Some(true),
        500 => Some(false),
        _ => None,
    }
}

/// O que mandar de um arquivo. Mãos: se o começo do arquivo continua igual
/// ao que já foi enviado, só o trecho novo (a partir da última mão já
/// enviada) — antes, o arquivo do dia inteiro ia de novo a cada 5 minutos
/// enquanto a sessão rolava. Torneio: sempre o arquivo inteiro (é pequeno
/// e o backend precisa dele todo).
fn trecho_para_enviar(kind: FileKind, texto: &str, antes: Option<SentText>) -> &str {
    if kind == FileKind::TournamentSummary {
        return texto;
    }
    match antes {
        Some(a) => {
            let len = a.len as usize;
            let mesmo_comeco =
                len <= texto.len() && texto.is_char_boundary(len) && fingerprint(&texto[..len]) == a.fingerprint;
            if mesmo_comeco {
                new_part_since(texto, len)
            } else {
                texto
            }
        }
        None => texto,
    }
}

/// Fila de pedaços de um lote recusado: cada recusa divide o pedaço ao
/// meio, até sobrar o item culpado sozinho (ver `Envio::mandar_lote`).
struct Isolamento {
    fila: VecDeque<(Vec<SyncFile>, Vec<usize>)>,
}

impl Isolamento {
    fn new(itens: Vec<SyncFile>, donos: Vec<usize>) -> Self {
        Isolamento {
            fila: VecDeque::from([(itens, donos)]),
        }
    }

    fn proximo(&mut self) -> Option<(Vec<SyncFile>, Vec<usize>)> {
        self.fila.pop_front()
    }

    /// O pedaço foi recusado. Com mais de um item, volta pra fila em duas
    /// metades (na mesma ordem) e devolve `None`; com um só, devolve o
    /// arquivo de onde ele veio.
    fn recusado(&mut self, mut itens: Vec<SyncFile>, mut donos: Vec<usize>) -> Option<usize> {
        if itens.len() <= 1 {
            return donos.first().copied();
        }
        let meio = itens.len() / 2;
        let (resto, resto_donos) = (itens.split_off(meio), donos.split_off(meio));
        self.fila.push_front((resto, resto_donos));
        self.fila.push_front((itens, donos));
        None
    }
}

/// Contexto de envio de um tipo de arquivo — guarda a chave atual e renova
/// uma vez se o site responder 401 no meio.
struct Envio<'a> {
    app: &'a AppHandle,
    device: &'a DeviceInfo,
    kind: FileKind,
    chave: String,
    client: SyncClient,
    renovou: bool,
}

impl Envio<'_> {
    async fn mandar(&mut self, room: &str, files: &[SyncFile], resumo: &mut SyncSummary) -> Result<(), Falha> {
        let mut esperas = 0;
        loop {
            let tentativa = match self.kind {
                FileKind::HandHistory => self
                    .client
                    .sync_batch(self.device, room, files)
                    .await
                    .map(|r| (r.total_hands, r.imported, r.duplicates, r.errors, r.skipped_by_date)),
                FileKind::TournamentSummary => self
                    .client
                    .sync_tournament_batch(self.device, room, files)
                    .await
                    .map(|r| (r.total_files, r.imported, r.duplicates, r.errors, r.skipped_by_date)),
            };
            match tentativa {
                Ok((total, imported, duplicates, errors, ignored)) => {
                    resumo.total_items += total;
                    resumo.imported += imported;
                    resumo.duplicates += duplicates;
                    resumo.errors += errors;
                    resumo.ignored_by_date += ignored;
                    return Ok(());
                }
                Err(e) if e.status() == Some(401) && !self.renovou => {
                    self.renovou = true;
                    self.chave = chave_de_acesso(self.app, Some(&self.chave)).await.map_err(Falha::Auth)?;
                    self.client = SyncClient::new(config::site_url(), self.chave.clone());
                }
                // Limite de envios por minuto do site: espera o que ele
                // pediu e segue, em vez de largar o ciclo no meio (num
                // histórico grande isso acontecia a cada poucos lotes).
                Err(e) if e.status() == Some(429) && esperas < MAX_ESPERAS_LIMITE => {
                    esperas += 1;
                    let segundos = e.retry_after().unwrap_or(30).clamp(1, 60);
                    tokio::time::sleep(Duration::from_secs(segundos)).await;
                }
                Err(e) => return Err(Falha::Site(e)),
            }
        }
    }

    /// Manda um lote. Se o site recusar por causa de algum arquivo (ver
    /// `recusa_por_arquivo`), divide o lote ao meio e tenta cada metade, até
    /// achar o arquivo culpado — que fica de lado (`recusados`) quando o
    /// site o recusa pelo conteúdo, ou depois de algumas recusas seguidas.
    /// Antes, um arquivo recusado travava tudo: o mesmo lote ia de novo a
    /// cada ciclo, pra sempre, e nada depois dele chegava ao site.
    ///
    /// `ao_recusar(dono, por_conteudo, falha)` decide o que fazer com o
    /// arquivo culpado: `true` = fica de lado e o resto segue, `false` =
    /// para o envio (tenta no próximo ciclo).
    async fn mandar_lote(
        &mut self,
        room: &str,
        files: Vec<SyncFile>,
        donos: Vec<usize>,
        resumo: &mut SyncSummary,
        ao_recusar: &mut (dyn FnMut(usize, bool, &Falha) -> bool + Send),
    ) -> Result<(), Falha> {
        let mut isolamento = Isolamento::new(files, donos);
        while let Some((files, donos)) = isolamento.proximo() {
            let falha = match self.mandar(room, &files, resumo).await {
                Ok(()) => continue,
                Err(f) => f,
            };
            let Some(por_conteudo) = recusa_por_arquivo(&falha) else {
                return Err(falha);
            };
            let Some(dono) = isolamento.recusado(files, donos) else {
                continue;
            };
            if !ao_recusar(dono, por_conteudo, &falha) {
                return Err(falha);
            }
        }
        Ok(())
    }
}

/// O que fazer com um arquivo do computador que o site recusou: conta a
/// recusa no progresso da sala (`SyncState::record_failure`) e, se for pra
/// deixar de lado, anota em `recusados` pra ele não ser marcado como enviado.
fn recusa_no_computador<'a>(
    pendentes: &'a [DiscoveredFile],
    sync_state: &'a mut SyncState,
    state_path: &'a Path,
    recusados: &'a mut HashSet<PathBuf>,
) -> impl FnMut(usize, bool, &Falha) -> bool + Send + 'a {
    move |dono, por_conteudo, falha| {
        let arquivo = &pendentes[dono];
        let desistiu = sync_state.record_failure(arquivo.path.clone(), arquivo.signature, por_conteudo, now_unix());
        salvar_progresso(sync_state, state_path);
        if desistiu {
            eprintln!(
                "[radar] o PokerSync recusou {} ({}) — deixado de lado, o resto segue",
                arquivo.path.display(),
                falha.mensagem()
            );
            recusados.insert(arquivo.path.clone());
        }
        desistiu
    }
}

/// Envia um tipo de arquivo (mãos ou torneios) de todas as salas, em lotes
/// que respeitam os limites do servidor (quantidade e tamanho) e com
/// arquivos grandes partidos no começo de uma mão. Lê um arquivo por vez —
/// antes lia todos os pendentes de uma vez pra memória.
async fn sincronizar_tipo(app: &AppHandle, kind: FileKind, device: &DeviceInfo) -> Result<Vec<SyncSummary>, Falha> {
    let state = app.state::<AppState>();
    let chave = chave_de_acesso(app, None).await.map_err(Falha::Auth)?;
    let mut envio = Envio {
        app,
        device,
        kind,
        client: SyncClient::new(config::site_url(), chave.clone()),
        chave,
        renovou: false,
    };

    let mut out = Vec::new();
    for (room, found) in discover_all(&state, kind) {
        let state_path = state.state_dir.join(state_file_name(room, kind));
        let mut sync_state = SyncState::load(&state_path);
        let mut resumo = SyncSummary {
            room: room.slug().to_string(),
            ..Default::default()
        };
        let pendentes: Vec<DiscoveredFile> = pending_files(&found, &sync_state).into_iter().cloned().collect();
        if !envio_liberado(room) {
            resumo.files_waiting = pendentes.len() as u32;
            out.push(resumo);
            continue;
        }
        let mut lote = BatchBuilder::new(MAX_FILES_PER_BATCH, MAX_BATCH_BYTES);
        // De qual arquivo (índice em `pendentes`) veio cada item do lote.
        let mut donos: Vec<usize> = Vec::new();
        let mut concluidos: Vec<Concluido> = Vec::new();
        let mut recusados: HashSet<PathBuf> = HashSet::new();

        for (indice, arquivo) in pendentes.iter().enumerate() {
            // Apagado/movido no meio da varredura: fica pra próxima volta.
            let Ok(texto) = read_text(&arquivo.path) else {
                continue;
            };
            let trecho = trecho_para_enviar(kind, &texto, sync_state.sent_text(&arquivo.path));
            let partes: Vec<&str> = match kind {
                FileKind::HandHistory => split_into_parts(trecho, MAX_HAND_PART_BYTES),
                // Resumo de torneio não pode ser partido; grande demais não
                // é um resumo de verdade e fica de fora.
                FileKind::TournamentSummary if trecho.len() > MAX_SUMMARY_BYTES => Vec::new(),
                FileKind::TournamentSummary => vec![trecho],
            };
            for parte in partes.into_iter().filter(|p| !p.trim().is_empty()) {
                if lote.is_full_for(parte.len()) {
                    let itens = lote.take();
                    let itens_donos = std::mem::take(&mut donos);
                    {
                        let mut ao_recusar = recusa_no_computador(&pendentes, &mut sync_state, &state_path, &mut recusados);
                        envio
                            .mandar_lote(room.slug(), itens, itens_donos, &mut resumo, &mut ao_recusar)
                            .await?;
                    }
                    resumo.files_synced += marcar_enviados(&mut sync_state, &state_path, &mut concluidos, &recusados);
                }
                lote.push(SyncFile {
                    raw_text: parte.to_string(),
                    captured_at: None,
                    file_name: arquivo.path.file_name().map(|n| n.to_string_lossy().into_owned()),
                });
                donos.push(indice);
            }
            concluidos.push(Concluido {
                path: arquivo.path.clone(),
                signature: arquivo.signature,
                enviado: SentText {
                    len: texto.len() as u64,
                    fingerprint: fingerprint(&texto),
                },
            });
        }
        if !lote.is_empty() {
            let itens = lote.take();
            let itens_donos = std::mem::take(&mut donos);
            let mut ao_recusar = recusa_no_computador(&pendentes, &mut sync_state, &state_path, &mut recusados);
            envio
                .mandar_lote(room.slug(), itens, itens_donos, &mut resumo, &mut ao_recusar)
                .await?;
            drop(ao_recusar);
        }
        resumo.files_synced += marcar_enviados(&mut sync_state, &state_path, &mut concluidos, &recusados);
        resumo.files_rejected = sync_state.given_up_count() as u32;
        out.push(resumo);
    }
    Ok(out)
}

/// Resultado de um ciclo — decide quanto esperar até o próximo.
enum Ciclo {
    Ok,
    SemSessao,
    SemInternet,
    /// Site pediu pra esperar: falta escolher o que importar ou o plano não inclui o Radar.
    Pausado,
    Erro,
}

/// Registra a falha no status (a tela mostra) e diz que tipo de ciclo foi.
fn registrar_falha(app: &AppHandle, falha: Falha) -> Ciclo {
    match falha {
        // Status já atualizado em `sessao_expirou` (ou nunca houve login).
        Falha::Auth(AuthError::NaoLogado) | Falha::Auth(AuthError::Expirada) => Ciclo::SemSessao,
        Falha::Auth(e @ AuthError::SemInternet) => {
            let mensagem = e.mensagem();
            atualizar_status(app, |s| {
                s.conexao = "sem_internet";
                s.ultimo_erro = Some(mensagem);
            });
            Ciclo::SemInternet
        }
        Falha::Site(e) => match e.code() {
            Some("IMPORT_SCOPE_NAO_DEFINIDO") => {
                atualizar_status(app, |s| {
                    s.conexao = "ok";
                    s.import_scope = None;
                    s.ultimo_erro = None;
                });
                Ciclo::Pausado
            }
            Some("RADAR_FORA_DO_PLANO") => {
                atualizar_status(app, |s| {
                    s.conexao = "ok";
                    s.radar_liberado = Some(false);
                    s.ultimo_erro = None;
                });
                Ciclo::Pausado
            }
            _ => {
                let sem_rede = matches!(e, SyncError::Network(_));
                let passageira = e.is_transient();
                let mensagem = e.to_string();
                atualizar_status(app, |s| {
                    if sem_rede {
                        s.conexao = "sem_internet";
                    }
                    s.ultimo_erro = Some(mensagem);
                });
                if passageira {
                    Ciclo::SemInternet
                } else {
                    Ciclo::Erro
                }
            }
        },
    }
}

/// Um ciclo: confere a sessão, manda o sinal de vida (o site guarda quando
/// este computador foi visto e responde o que o jogador escolheu importar
/// e se o plano inclui o Radar) e, se `enviar`, manda mãos e torneios novos.
async fn ciclo(app: &AppHandle, enviar: bool) -> Ciclo {
    let state = app.state::<AppState>();
    if !state.logado.load(Ordering::SeqCst) {
        return Ciclo::SemSessao;
    }
    let _um_por_vez = state.sync_lock.lock().await;
    atualizar_status(app, |s| s.sincronizando = true);
    let resultado = ciclo_interno(app, enviar).await;
    atualizar_status(app, |s| s.sincronizando = false);
    atualizar_vigia(app);
    resultado
}

/// Quanto cada escolha do que importar abrange — "tudo" > "últimos 3
/// meses" > "só de agora em diante".
fn abrangencia(escopo: Option<&str>) -> u8 {
    match escopo {
        Some("from_now") => 1,
        Some("last_3_months") => 2,
        Some("full_history") => 3,
        _ => 0,
    }
}

/// Guarda a escolha atual do que importar e, se o jogador AMPLIOU a
/// escolha desde a última vez (ex.: de "só de agora" pra "tudo"), esquece
/// o que já foi enviado: os arquivos antigos tinham sido marcados como
/// enviados mesmo com as mãos de antes do corte descartadas pelo site —
/// sem isso, o histórico nunca chegaria. O site descarta as repetidas.
/// Roda dentro do ciclo, com o envio travado (`sync_lock`).
fn lembrar_escopo(app: &AppHandle, atual: Option<&str>) {
    let Some(atual) = atual else { return };
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    let anterior = cfg.escopo_sincronizado.clone();
    if anterior.as_deref() == Some(atual) {
        return;
    }
    if anterior.is_some() && abrangencia(Some(atual)) > abrangencia(anterior.as_deref()) {
        if let Err(e) = std::fs::remove_dir_all(&state.state_dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[radar] não consegui recomeçar o envio: {e}");
                return;
            }
        }
        eprintln!("[radar] escolha ampliada ({anterior:?} -> {atual}): reenviando o histórico");
    }
    cfg.escopo_sincronizado = Some(atual.to_string());
    let _ = cfg.save(&state.config_path);
}

async fn ciclo_interno(app: &AppHandle, enviar: bool) -> Ciclo {
    let device = device_info(&app.state::<AppState>());
    let d = &device;
    let info = match chamar_site(app, |c| async move { c.ping(Some(d)).await }).await {
        Ok(info) => info,
        Err(f) => return registrar_falha(app, f),
    };
    atualizar_status(app, |s| {
        s.conexao = "ok";
        s.import_scope = info.import_scope.clone();
        s.radar_liberado = info.radar_liberado;
        s.ultimo_erro = None;
    });
    if info.radar_liberado == Some(false) || info.import_scope.is_none() {
        return Ciclo::Pausado;
    }
    lembrar_escopo(app, info.import_scope.as_deref());
    if !enviar {
        return Ciclo::Ok;
    }

    let mut novidades = 0u32;
    let mut recusados = 0u32;
    for kind in [FileKind::HandHistory, FileKind::TournamentSummary] {
        match sincronizar_tipo(app, kind, &device).await {
            Ok(resumos) => {
                novidades += resumos.iter().map(|r| r.imported).sum::<u32>();
                recusados += resumos.iter().map(|r| r.files_rejected).sum::<u32>();
            }
            Err(f) => return registrar_falha(app, f),
        }
    }
    let quando = agora();
    atualizar_status(app, |s| {
        s.ultima_sincronizacao = Some(quando);
        s.ultimo_erro = None;
        s.arquivos_recusados = recusados;
        if novidades > 0 {
            s.ultimas_novidades = Some(novidades);
            s.ultimas_novidades_em = Some(quando);
        }
    });
    Ciclo::Ok
}

/// Quanto o Radar encontrou no computador — mostrado na pergunta "o que
/// importar" (primeiro login), pra o jogador decidir com números de
/// verdade (pedido explícito: a escolha fica no Radar e no site).
#[derive(serde::Serialize, Clone, Default)]
struct ResumoComputador {
    maos: u64,
    maos_ultimos_3_meses: u64,
    torneios: u64,
    torneios_ultimos_3_meses: u64,
    /// Mês/ano da mão ou torneio mais antigo com data ("03/2024").
    mais_antigo: Option<String>,
}

fn resumir_computador(state: &AppState) -> ResumoComputador {
    let hoje = (agora() / 86_400) as i64;
    // ~3 meses; a conta exata fica no site (corte = hora da escolha − 3 meses).
    let corte = hoje - 92;
    let mut r = ResumoComputador::default();
    let mut mais_antigo: Option<i64> = None;
    let mut anotar = |dia: Option<i64>, recente: &mut u64| {
        if let Some(d) = dia {
            if d >= corte {
                *recente += 1;
            }
            mais_antigo = Some(mais_antigo.map_or(d, |m: i64| m.min(d)));
        }
    };
    for (_, arquivos) in discover_all(state, FileKind::HandHistory) {
        for f in arquivos {
            let Ok(texto) = read_text(&f.path) else { continue };
            for dia in scanner::text::hand_days(&texto) {
                r.maos += 1;
                anotar(dia, &mut r.maos_ultimos_3_meses);
            }
        }
    }
    for (_, arquivos) in discover_all(state, FileKind::TournamentSummary) {
        for f in arquivos {
            let Ok(texto) = read_text(&f.path) else { continue };
            r.torneios += 1;
            anotar(texto.lines().find_map(scanner::text::day_in_line), &mut r.torneios_ultimos_3_meses);
        }
    }
    r.mais_antigo = mais_antigo.map(|d| {
        let (ano, mes, _) = scanner::text::civil_from_days(d);
        format!("{mes:02}/{ano}")
    });
    r
}

/// Conta o que tem no computador (lê os arquivos — pode levar alguns
/// segundos num histórico grande, por isso roda fora da thread da tela).
#[tauri::command]
async fn resumo_do_computador(app: AppHandle) -> Result<ResumoComputador, String> {
    tauri::async_runtime::spawn_blocking(move || resumir_computador(&app.state::<AppState>()))
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Painel "Suas salas" — o "Site Settings" do PokerTracker/Holdem Manager:
// pra cada sala, se a pasta existe, quanto tem nela e o que fazer quando
// não acha nada.
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Clone, Debug, Default)]
struct PainelSala {
    slug: String,
    nome: String,
    /// Pastas onde o Radar procura essa sala e que existem no disco (as
    /// padrão dela, mais as adicionadas onde apareceram arquivos dela).
    pastas: Vec<String>,
    maos: u32,
    maos_pendentes: u32,
    torneios: u32,
    torneios_pendentes: u32,
    recusados: u32,
    /// "ok" | "pendente" | "aguardando_site" | "pasta_vazia" | "nao_encontrada"
    situacao: &'static str,
    dica: Option<String>,
}

fn situacao_da_sala(room: PokerRoom, p: &PainelSala) -> &'static str {
    if p.maos + p.torneios > 0 {
        if !envio_liberado(room) {
            "aguardando_site"
        } else if p.maos_pendentes + p.torneios_pendentes > 0 {
            "pendente"
        } else {
            "ok"
        }
    } else if !p.pastas.is_empty() {
        "pasta_vazia"
    } else {
        "nao_encontrada"
    }
}

/// O que dizer pro jogador sobre a sala — sempre uma ação que ele pode
/// fazer, nunca só "não achei".
fn dica_da_sala(room: PokerRoom, p: &PainelSala) -> Option<String> {
    let nome = room.display_name();
    if room == PokerRoom::GgPoker && p.maos == 0 {
        return Some(
            "O GGPoker costuma deixar o histórico no PokerCraft, e não no computador: baixe lá suas mãos e \
             torneios e use \"Importar arquivos baixados\"."
                .to_string(),
        );
    }
    match p.situacao {
        "nao_encontrada" => Some(format!(
            "Não achei a pasta do {nome} neste computador. Se você joga lá, ative no cliente a opção de \
             salvar o histórico de mãos, ou adicione a pasta em \"Importar mãos\"."
        )),
        "pasta_vazia" => Some(format!(
            "Achei a pasta do {nome}, mas nenhuma mão nela. Confira se o cliente está salvando o histórico de mãos."
        )),
        "aguardando_site" => Some(format!(
            "O envio do {nome} começa assim que o PokerSync aprender a ler essa sala — nada se perde até lá."
        )),
        _ if p.recusados > 0 => Some(format!(
            "{} arquivo(s) recusado(s) pelo PokerSync — o Radar tenta de novo em um dia.",
            p.recusados
        )),
        _ if room == PokerRoom::GgPoker && p.torneios == 0 => Some(
            "Os resultados de torneio do GGPoker ficam no PokerCraft: baixe lá e use \"Importar arquivos baixados\"."
                .to_string(),
        ),
        _ if room == PokerRoom::PokerStars && p.torneios == 0 => Some(
            "Joga torneios? No cliente do PokerStars, ative também a opção de salvar os resumos de torneio — \
             é de lá que vem a premiação."
                .to_string(),
        ),
        _ => None,
    }
}

fn montar_painel(state: &AppState) -> Vec<PainelSala> {
    let mut painel: Vec<(PokerRoom, PainelSala, Vec<PathBuf>)> = PokerRoom::ALL
        .into_iter()
        .map(|room| {
            let p = PainelSala {
                slug: room.slug().to_string(),
                nome: room.display_name().to_string(),
                ..Default::default()
            };
            (room, p, Vec::new())
        })
        .collect();

    for kind in [FileKind::HandHistory, FileKind::TournamentSummary] {
        let roots = search_roots(kind, &pastas_extras(state, kind));
        for (room, arquivos) in discover_all(state, kind) {
            let Some((_, p, pastas)) = painel.iter_mut().find(|(r, _, _)| *r == room) else {
                continue;
            };
            let sync_state = SyncState::load(&state.state_dir.join(state_file_name(room, kind)));
            let pendentes = pending_files(&arquivos, &sync_state).len() as u32;
            p.recusados += sync_state.given_up_count() as u32;
            match kind {
                FileKind::HandHistory => (p.maos, p.maos_pendentes) = (arquivos.len() as u32, pendentes),
                FileKind::TournamentSummary => (p.torneios, p.torneios_pendentes) = (arquivos.len() as u32, pendentes),
            }
            // Pastas padrão da sala que existem, mesmo vazias (cliente
            // instalado sem salvar histórico), e as adicionadas onde
            // apareceram arquivos dela.
            pastas.extend(
                roots
                    .iter()
                    .filter(|r| r.hint == Some(room) && r.path.is_dir())
                    .map(|r| r.path.clone()),
            );
            for f in &arquivos {
                if let Some(r) = roots.iter().find(|r| f.path.starts_with(&r.path)) {
                    pastas.push(r.path.clone());
                }
            }
        }
    }

    painel
        .into_iter()
        .map(|(room, mut p, mut pastas)| {
            pastas.sort();
            pastas.dedup();
            p.pastas = pastas.into_iter().map(|c| c.display().to_string()).collect();
            p.situacao = situacao_da_sala(room, &p);
            p.dica = dica_da_sala(room, &p);
            p
        })
        .collect()
}

/// Situação de cada sala no computador (usa a memória de reconhecimento —
/// rápido mesmo com anos de histórico).
#[tauri::command]
async fn painel_salas(app: AppHandle) -> Result<Vec<PainelSala>, String> {
    tauri::async_runtime::spawn_blocking(move || montar_painel(&app.state::<AppState>()))
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// "Importar arquivos baixados" (zip do PokerCraft ou .txt soltos)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct LinhaImportacao {
    tipo: &'static str,
    sala: String,
    arquivos: u32,
    imported: u32,
    duplicates: u32,
    errors: u32,
    ignored_by_date: u32,
    recusados: u32,
    /// Sala que o site ainda não sabe ler (ver `envio_liberado`): nada enviado.
    aguardando: bool,
}

#[derive(serde::Serialize)]
struct ResultadoImportacao {
    arquivos_lidos: u32,
    ignorados: u32,
    linhas: Vec<LinhaImportacao>,
}

/// Lê os arquivos escolhidos, reconhece sala e tipo de cada um e envia —
/// mesmos lotes, limites e tratamento de recusa do envio automático. Não
/// entra no progresso das pastas: o site descarta o que já tiver.
#[tauri::command]
async fn importar_arquivos(app: AppHandle, caminhos: Vec<String>) -> Result<ResultadoImportacao, String> {
    let state = app.state::<AppState>();
    let _um_por_vez = state.sync_lock.lock().await;
    atualizar_status(&app, |s| s.sincronizando = true);
    let resultado = importar_arquivos_interno(&app, caminhos).await;
    atualizar_status(&app, |s| s.sincronizando = false);
    resultado
}

async fn importar_arquivos_interno(app: &AppHandle, caminhos: Vec<String>) -> Result<ResultadoImportacao, String> {
    let caminhos: Vec<PathBuf> = caminhos.into_iter().map(PathBuf::from).collect();
    let leitura = tauri::async_runtime::spawn_blocking(move || importar::ler_escolhidos(&caminhos))
        .await
        .map_err(|e| e.to_string())??;
    let device = device_info(&app.state::<AppState>());
    let mut linhas = Vec::new();

    for grupo in leitura.grupos {
        let mut linha = LinhaImportacao {
            tipo: grupo.kind.slug(),
            sala: grupo.room.slug().to_string(),
            arquivos: grupo.textos.len() as u32,
            imported: 0,
            duplicates: 0,
            errors: 0,
            ignored_by_date: 0,
            recusados: 0,
            aguardando: false,
        };
        if !envio_liberado(grupo.room) {
            linha.aguardando = true;
            linhas.push(linha);
            continue;
        }
        let chave = match chave_de_acesso(app, None).await {
            Ok(c) => c,
            Err(e) => return Err(e.mensagem()),
        };
        let mut envio = Envio {
            app,
            device: &device,
            kind: grupo.kind,
            client: SyncClient::new(config::site_url(), chave.clone()),
            chave,
            renovou: false,
        };
        let mut resumo = SyncSummary::default();
        let mut recusados: HashSet<usize> = HashSet::new();
        let mut lote = BatchBuilder::new(MAX_FILES_PER_BATCH, MAX_BATCH_BYTES);
        let mut donos: Vec<usize> = Vec::new();
        let enviado = async {
            for (indice, texto) in grupo.textos.iter().enumerate() {
                let partes: Vec<&str> = match grupo.kind {
                    FileKind::HandHistory => split_into_parts(texto, MAX_HAND_PART_BYTES),
                    FileKind::TournamentSummary if texto.len() > MAX_SUMMARY_BYTES => {
                        recusados.insert(indice);
                        Vec::new()
                    }
                    FileKind::TournamentSummary => vec![texto.as_str()],
                };
                for parte in partes.into_iter().filter(|p| !p.trim().is_empty()) {
                    if lote.is_full_for(parte.len()) {
                        let (itens, itens_donos) = (lote.take(), std::mem::take(&mut donos));
                        let mut ao_recusar = |dono: usize, _: bool, _: &Falha| {
                            recusados.insert(dono);
                            true
                        };
                        envio
                            .mandar_lote(grupo.room.slug(), itens, itens_donos, &mut resumo, &mut ao_recusar)
                            .await?;
                    }
                    lote.push(SyncFile {
                        raw_text: parte.to_string(),
                        captured_at: None,
                        file_name: None,
                    });
                    donos.push(indice);
                }
            }
            if !lote.is_empty() {
                let (itens, itens_donos) = (lote.take(), std::mem::take(&mut donos));
                let mut ao_recusar = |dono: usize, _: bool, _: &Falha| {
                    recusados.insert(dono);
                    true
                };
                envio
                    .mandar_lote(grupo.room.slug(), itens, itens_donos, &mut resumo, &mut ao_recusar)
                    .await?;
            }
            Ok::<(), Falha>(())
        }
        .await;
        if let Err(f) = enviado {
            let mensagem = f.mensagem();
            registrar_falha(app, f);
            return Err(mensagem);
        }
        linha.imported = resumo.imported;
        linha.duplicates = resumo.duplicates;
        linha.errors = resumo.errors;
        linha.ignored_by_date = resumo.ignored_by_date;
        linha.recusados = recusados.len() as u32;
        linhas.push(linha);
    }

    let novidades: u32 = linhas.iter().map(|l| l.imported).sum();
    if novidades > 0 {
        let quando = agora();
        atualizar_status(app, |s| {
            s.ultimas_novidades = Some(novidades);
            s.ultimas_novidades_em = Some(quando);
        });
    }
    Ok(ResultadoImportacao {
        arquivos_lidos: leitura.arquivos_lidos,
        ignorados: leitura.ignorados,
        linhas,
    })
}

/// O jogador escolheu, no próprio Radar, o que importar — salva no site
/// (mesma escolha da tela do Radar lá) e já começa a enviar.
#[tauri::command]
async fn escolher_importacao(app: AppHandle, escopo: String) -> Result<RadarStatus, String> {
    if abrangencia(Some(&escopo)) == 0 {
        return Err("Escolha inválida.".to_string());
    }
    let e = escopo.as_str();
    match chamar_site(&app, |c| async move { c.set_import_scope(e).await }).await {
        Ok(()) => {
            atualizar_status(&app, |s| {
                s.import_scope = Some(escopo.clone());
                s.ultimo_erro = None;
            });
            app.state::<AppState>().wake.notify_one();
            Ok(app.state::<AppState>().status.lock().unwrap().clone())
        }
        Err(f) => {
            let mensagem = f.mensagem();
            registrar_falha(&app, f);
            Err(mensagem)
        }
    }
}

/// "Sincronizar agora" (tela ou menu do ícone): um ciclo completo na hora,
/// mesmo com a sincronização automática desligada.
#[tauri::command]
async fn sincronizar_agora(app: AppHandle) -> Result<RadarStatus, String> {
    let resultado = ciclo(&app, true).await;
    let status = app.state::<AppState>().status.lock().unwrap().clone();
    match resultado {
        Ciclo::SemSessao => Err(AuthError::NaoLogado.mensagem()),
        Ciclo::SemInternet | Ciclo::Erro => Err(status
            .ultimo_erro
            .clone()
            .unwrap_or_else(|| "Não foi possível sincronizar agora.".to_string())),
        Ciclo::Ok | Ciclo::Pausado => Ok(status),
    }
}

/// "Verificar agora" do painel de mãos/torneios: envia só aquele tipo e
/// devolve o resultado por sala.
#[tauri::command]
async fn sync_now(app: AppHandle, kind: String) -> Result<Vec<SyncSummary>, String> {
    let kind = parse_kind(&kind)?;
    let state = app.state::<AppState>();
    let _um_por_vez = state.sync_lock.lock().await;
    let device = device_info(&state);
    atualizar_status(&app, |s| s.sincronizando = true);
    let resultado = sincronizar_tipo(&app, kind, &device).await;
    atualizar_status(&app, |s| s.sincronizando = false);
    match resultado {
        Ok(resumos) => {
            let novidades: u32 = resumos.iter().map(|r| r.imported).sum();
            let quando = agora();
            atualizar_status(&app, |s| {
                s.ultima_sincronizacao = Some(quando);
                s.ultimo_erro = None;
                if novidades > 0 {
                    s.ultimas_novidades = Some(novidades);
                    s.ultimas_novidades_em = Some(quando);
                }
            });
            Ok(resumos)
        }
        Err(f) => {
            let mensagem = f.mensagem();
            registrar_falha(&app, f);
            Err(mensagem)
        }
    }
}

/// Ciclo automático em background: o primeiro roda logo depois de abrir
/// (antes esperava 5 minutos), os seguintes a cada `AUTO_SYNC_INTERVAL`.
/// Sem internet (ex.: logo depois de ligar o PC), tenta de novo em 20 s,
/// 40 s, 80 s... até o intervalo normal. Com a sincronização automática
/// desligada, só manda o sinal de vida.
fn spawn_ciclo_automatico(app: AppHandle) {
    let intervalo = config::dev_interval_secs().map(Duration::from_secs).unwrap_or(AUTO_SYNC_INTERVAL);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut espera_falha = RETRY_INICIAL.min(intervalo);
        loop {
            let enviar = app.state::<AppState>().config.lock().unwrap().auto_sync_enabled;
            let espera = match ciclo(&app, enviar).await {
                Ciclo::SemInternet => {
                    let e = espera_falha;
                    espera_falha = (espera_falha * 2).min(intervalo);
                    e
                }
                _ => {
                    espera_falha = RETRY_INICIAL.min(intervalo);
                    intervalo
                }
            };
            let state = app.state::<AppState>();
            tokio::select! {
                _ = tokio::time::sleep(espera) => {}
                _ = state.wake.notified() => {}
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Tem que ser o primeiro plugin. Um segundo clique no atalho (ou o
        // link de volta do login com Google, que no Windows abre o
        // programa de novo) não cria outra cópia: só traz esta pra frente
        // e repassa o link. Duas cópias rodando renovavam a mesma sessão
        // em paralelo — o Supabase entende isso como chave roubada e
        // derruba o login.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            mostrar_janela(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // Argumento passado quando o SO abre o app sozinho no login —
            // usado no setup() abaixo pra abrir só na bandeja em vez de
            // estourar a janela na cara do usuário todo boot.
            Some(vec!["--hidden"]),
        ))
        .setup(|app| {
            let config_dir = app.path().app_config_dir().expect("sem app_config_dir");
            let config_path = config_dir.join("config.json");
            let state_dir = config_dir.join("sync-state");
            let mut config = AppConfig::load(&config_path);
            let refresh_token = config::dev_refresh_token().or_else(keychain::load_refresh_token);
            let logado = refresh_token.is_some();

            // Quem já usava o Radar antes da 0.2.0 e continua logado: liga o
            // "iniciar com o computador" uma vez, como no primeiro login.
            if logado && !config.autostart_configurado && app.autolaunch().enable().is_ok() {
                config.autostart_configurado = true;
                let _ = config.save(&config_path);
            }

            let status = RadarStatus::inicial(logado, config.sessao_expirada);
            let dica = texto_bandeja(&status);
            let (aviso_vigia, avisos) = tokio::sync::mpsc::unbounded_channel();
            app.manage(AppState {
                config_path,
                state_dir,
                config: Mutex::new(config),
                pending_google_state: Mutex::new(None),
                session: tokio::sync::Mutex::new(refresh_token.map(|refresh_token| Session {
                    access_token: None,
                    refresh_token,
                })),
                logado: AtomicBool::new(logado),
                sync_lock: tokio::sync::Mutex::new(()),
                discover_lock: Mutex::new(()),
                vigia: Mutex::new(watcher::Vigia::default()),
                aviso_vigia,
                status: Mutex::new(status),
                wake: tokio::sync::Notify::new(),
            });

            // Confere o login, manda o sinal de vida e sincroniza logo ao
            // abrir; depois, a cada 5 minutos.
            spawn_ciclo_automatico(app.handle().clone());
            spawn_vigia(app.handle().clone(), avisos);
            atualizar_vigia(app.handle());

            // Login com Google: radar-pokersync://auth?code=...&state=...
            // volta aqui depois do navegador do sistema completar o OAuth
            // (ver start_google_login e app/agent-login no produto). O
            // esquema vem de `plugins.deep-link.desktop.schemes` no
            // tauri.conf.json — antes estava em `plugins.deep-link.schemes`,
            // que o plugin ignora, e nada era registrado. `register_all()`
            // reforça o registro toda vez que o app abre (no Windows,
            // reescreve a chave apontando pro executável atual). Continua
            // existindo o `paste_login_link` como caminho manual.
            if let Err(e) = app.deep_link().register_all() {
                eprintln!("[radar] não consegui registrar o link de login: {e}");
            }

            let deep_link_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let Some((code, received_state)) = parse_auth_deep_link(&url) else {
                        continue;
                    };
                    eprintln!("[radar] link de login recebido");
                    let handle = deep_link_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        complete_google_login(handle, code, received_state).await;
                    });
                }
            });

            let show_i = MenuItem::with_id(app, "show", "Abrir o Radar", true, None::<&str>)?;
            let sync_i = MenuItem::with_id(app, "sync", "Sincronizar agora", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Fechar o Radar", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_i, &sync_i, &quit_i])?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&tray_menu)
                // Clique esquerdo abre a janela; o direito mostra o menu.
                .show_menu_on_left_click(false)
                .tooltip(dica)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        mostrar_janela(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => mostrar_janela(app),
                    "sync" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            ciclo(&handle, true).await;
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // A janela nasce escondida (tauri.conf.json, "visible": false)
            // pra não piscar quando o sistema abre o Radar no boot
            // (--hidden): aí ele fica só no ícone perto do relógio.
            let launched_hidden = std::env::args().any(|a| a == "--hidden");
            if !launched_hidden {
                mostrar_janela(app.handle());
            }
            Ok(())
        })
        // Fechar a janela (X) esconde em vez de encerrar — o Radar é feito
        // pra ficar rodando em background. Encerrar de verdade é só pelo
        // menu do ícone ("Fechar o Radar").
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
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
            sincronizar_agora,
            resumo_do_computador,
            painel_salas,
            importar_arquivos,
            escolher_importacao,
            get_autostart,
            set_autostart,
            abrir_no_site,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao rodar o app Tauri");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand(n: u32) -> String {
        format!("PokerStars Hand #{n}: Tournament #1\nSeat 1: hero\n*** SUMMARY ***\n\n\n")
    }

    #[test]
    fn sends_only_new_hands_when_file_grew() {
        let antes: String = (1..=3).map(hand).collect();
        let registro = SentText {
            len: antes.len() as u64,
            fingerprint: fingerprint(&antes),
        };
        let depois = format!("{antes}{}", hand(4));
        let trecho = trecho_para_enviar(FileKind::HandHistory, &depois, Some(registro));
        assert!(trecho.starts_with("PokerStars Hand #3:"));
        assert!(trecho.contains("Hand #4:"));
        assert!(!trecho.contains("Hand #1:"));
    }

    #[test]
    fn sends_whole_file_when_beginning_changed() {
        let antes: String = (1..=3).map(hand).collect();
        let registro = SentText {
            len: antes.len() as u64,
            fingerprint: fingerprint(&antes),
        };
        let reescrito = format!("{}{}", hand(9), hand(10));
        assert_eq!(
            trecho_para_enviar(FileKind::HandHistory, &reescrito, Some(registro)),
            reescrito
        );
    }

    #[test]
    fn tournament_summaries_always_go_whole() {
        let texto = "PokerStars Tournament #1\nBuy-In: $10\n";
        let registro = SentText {
            len: 5,
            fingerprint: fingerprint(&texto[..5]),
        };
        assert_eq!(trecho_para_enviar(FileKind::TournamentSummary, texto, Some(registro)), texto);
    }

    fn rejeicao(status: u16, code: Option<&str>) -> Falha {
        Falha::Site(SyncError::Rejected {
            status,
            code: code.map(str::to_string),
            message: String::new(),
            retry_after: None,
        })
    }

    #[test]
    fn only_file_related_rejections_are_isolated() {
        assert_eq!(recusa_por_arquivo(&rejeicao(400, None)), Some(true));
        assert_eq!(recusa_por_arquivo(&rejeicao(413, None)), Some(true));
        assert_eq!(recusa_por_arquivo(&rejeicao(500, None)), Some(false));
        // Não é culpa de um arquivo: para e tenta no próximo ciclo.
        for status in [401, 403, 404, 429, 502, 503, 504] {
            assert_eq!(recusa_por_arquivo(&rejeicao(status, None)), None, "status {status}");
        }
        assert_eq!(recusa_por_arquivo(&rejeicao(409, Some("IMPORT_SCOPE_NAO_DEFINIDO"))), None);
        assert_eq!(recusa_por_arquivo(&Falha::Auth(AuthError::SemInternet)), None);
    }

    #[test]
    fn isolation_finds_the_rejected_file_and_sends_the_rest() {
        // 7 itens de 5 arquivos (o arquivo 2 tem três partes); o "site"
        // recusa qualquer pedaço que contenha a parte "ruim".
        let textos = ["a", "b", "c1", "ruim", "c3", "d", "e"];
        let donos = vec![0, 1, 2, 2, 2, 3, 4];
        let itens: Vec<SyncFile> = textos
            .iter()
            .map(|t| SyncFile {
                raw_text: t.to_string(),
                captured_at: None,
                file_name: None,
            })
            .collect();

        let mut isolamento = Isolamento::new(itens, donos);
        let mut aceitos = Vec::new();
        let mut culpados = Vec::new();
        let mut envios = 0;
        while let Some((itens, donos)) = isolamento.proximo() {
            envios += 1;
            if itens.iter().any(|i| i.raw_text == "ruim") {
                if let Some(dono) = isolamento.recusado(itens, donos) {
                    culpados.push(dono);
                }
            } else {
                aceitos.extend(itens.into_iter().map(|i| i.raw_text));
            }
        }
        assert_eq!(culpados, vec![2]);
        assert_eq!(aceitos, vec!["a", "b", "c1", "c3", "d", "e"]); // na ordem
        assert!(envios <= 7, "{envios} envios");
    }

    fn painel(maos: u32, torneios: u32, pastas: usize, pendentes: u32) -> PainelSala {
        PainelSala {
            maos,
            torneios,
            maos_pendentes: pendentes,
            pastas: vec!["C:/x".to_string(); pastas],
            ..Default::default()
        }
    }

    fn com_situacao(room: PokerRoom, mut p: PainelSala) -> PainelSala {
        p.situacao = situacao_da_sala(room, &p);
        p.dica = dica_da_sala(room, &p);
        p
    }

    #[test]
    fn room_panel_says_what_to_do() {
        let ps = com_situacao(PokerRoom::PokerStars, painel(100, 5, 1, 0));
        assert_eq!((ps.situacao, ps.dica.is_none()), ("ok", true));

        let ps_sem_resumo = com_situacao(PokerRoom::PokerStars, painel(100, 0, 1, 0));
        assert!(ps_sem_resumo.dica.unwrap().contains("resumos de torneio"));

        let pendente = com_situacao(PokerRoom::PokerStars, painel(100, 5, 1, 3));
        assert_eq!(pendente.situacao, "pendente");

        let vazia = com_situacao(PokerRoom::PartyPoker, painel(0, 0, 1, 0));
        assert_eq!(vazia.situacao, "pasta_vazia");
        assert!(vazia.dica.unwrap().contains("salvando o histórico"));

        let sumida = com_situacao(PokerRoom::Poker888, painel(0, 0, 0, 0));
        assert_eq!(sumida.situacao, "nao_encontrada");

        let gg = com_situacao(PokerRoom::GgPoker, painel(0, 0, 0, 0));
        assert!(gg.dica.unwrap().contains("PokerCraft"));

        // O ACR não espera mais o site: com mão pendente, é envio normal.
        let acr = com_situacao(PokerRoom::Acr, painel(40, 0, 1, 40));
        assert_eq!(acr.situacao, "pendente");
    }

    #[test]
    fn wider_choices_rank_higher() {
        assert!(abrangencia(Some("full_history")) > abrangencia(Some("last_3_months")));
        assert!(abrangencia(Some("last_3_months")) > abrangencia(Some("from_now")));
        assert!(abrangencia(Some("from_now")) > abrangencia(None));
        assert_eq!(abrangencia(Some("qualquer")), 0);
    }

    #[test]
    fn tray_text_explains_what_is_missing() {
        let mut s = RadarStatus::inicial(true, false);
        assert!(texto_bandeja(&s).ends_with("conectando…"));
        s.conexao = "ok";
        assert!(texto_bandeja(&s).ends_with("falta escolher no site o que importar"));
        s.import_scope = Some("from_now".into());
        assert!(texto_bandeja(&s).ends_with("sincronizando sozinho"));
        s.radar_liberado = Some(false);
        assert!(texto_bandeja(&s).ends_with("seu plano não inclui o Radar"));
        let expirada = RadarStatus::inicial(false, true);
        assert_eq!(expirada.conexao, "sessao_expirada");
    }
}
