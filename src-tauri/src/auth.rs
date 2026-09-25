//! Login do agente contra o GoTrue do Supabase (mesma auth usada pelo
//! produto web). Dois caminhos: email/senha via password grant aqui
//! embaixo, ou Google — que não roda dentro da janela nativa do Tauri,
//! então abre no navegador do sistema (ver `lib.rs::start_google_login`)
//! e volta pelo deep link `radar-pokersync://auth`. Em nenhum dos dois a
//! senha do usuário passa por aqui além do POST direto ao GoTrue; só a
//! chave de renovação resultante é guardada (no keychain, ver
//! `keychain.rs`).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use crate::config::{supabase_url, SUPABASE_ANON_KEY};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GoTrueUser {
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoTrueSession {
    access_token: String,
    refresh_token: String,
    user: GoTrueUser,
}

#[derive(Debug, Deserialize)]
struct GoTrueError {
    #[serde(alias = "error_description", alias = "msg")]
    message: Option<String>,
}

pub struct LoginResult {
    pub access_token: String,
    pub refresh_token: String,
    pub email: Option<String>,
}

const SEM_CONEXAO: &str = "Sem conexão com o PokerSync — confira a internet e tente de novo.";

/// Mensagens do GoTrue vêm em inglês — traduz as que o jogador vê de fato.
fn traduzir_erro_login(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("invalid login credentials") {
        "Email ou senha inválidos.".to_string()
    } else if lower.contains("email not confirmed") {
        "Confirme seu email antes de entrar (veja a caixa de entrada).".to_string()
    } else if lower.contains("rate limit") || lower.contains("too many") {
        "Muitas tentativas seguidas — espere um pouco e tente de novo.".to_string()
    } else {
        msg.to_string()
    }
}

pub async fn login_with_password(email: &str, password: &str) -> Result<LoginResult, String> {
    let client = sync_client::http_client();
    let url = format!("{}/auth/v1/token?grant_type=password", supabase_url());
    let resp = client
        .post(url)
        .header("apikey", SUPABASE_ANON_KEY)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|_| SEM_CONEXAO.to_string())?;

    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|_| SEM_CONEXAO.to_string())?;

    if !status.is_success() {
        let msg = serde_json::from_slice::<GoTrueError>(&bytes)
            .ok()
            .and_then(|e| e.message)
            .map(|m| traduzir_erro_login(&m))
            .unwrap_or_else(|| "Email ou senha inválidos.".to_string());
        return Err(msg);
    }

    let session: GoTrueSession = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(LoginResult {
        access_token: session.access_token,
        refresh_token: session.refresh_token,
        email: session.user.email,
    })
}

/// Por que a renovação falhou — a diferença importa: sem internet (ou
/// servidor instável) o Radar só tenta de novo mais tarde, mantendo o
/// login; chave recusada significa que a sessão acabou de verdade e o
/// jogador precisa entrar de novo.
#[derive(Debug)]
pub enum RefreshError {
    /// O Supabase recusou a chave (vencida, já usada ou sessão encerrada).
    Invalid,
    /// Rede, tempo limite, 429 ou 5xx — vale tentar de novo depois.
    Transient(String),
}

/// Troca a chave de renovação por uma chave de acesso nova (e uma chave de
/// renovação nova — o Supabase gira as duas a cada troca).
pub async fn refresh_session(refresh_token: &str) -> Result<LoginResult, RefreshError> {
    let client = sync_client::http_client();
    let url = format!("{}/auth/v1/token?grant_type=refresh_token", supabase_url());
    let resp = client
        .post(url)
        .header("apikey", SUPABASE_ANON_KEY)
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(RefreshError::Transient(format!("status {status}")));
    }
    if !status.is_success() {
        return Err(RefreshError::Invalid);
    }
    let session: GoTrueSession =
        serde_json::from_slice(&bytes).map_err(|e| RefreshError::Transient(e.to_string()))?;
    Ok(LoginResult {
        access_token: session.access_token,
        refresh_token: session.refresh_token,
        email: session.user.email,
    })
}

/// Encerra no servidor só a sessão deste Radar (`scope=local`) — antes o
/// "Sair" apagava a chave só do computador e ela continuava valendo no
/// servidor. Falha aqui não impede o "Sair" local.
pub async fn logout_remote(access_token: &str) {
    let client = sync_client::http_client();
    let url = format!("{}/auth/v1/logout?scope=local", supabase_url());
    let _ = client
        .post(url)
        .header("apikey", SUPABASE_ANON_KEY)
        .bearer_auth(access_token)
        .send()
        .await;
}

#[derive(Deserialize)]
struct Claims {
    email: Option<String>,
    exp: Option<i64>,
}

fn claims(access_token: &str) -> Option<Claims> {
    let payload_b64 = access_token.split('.').nth(1)?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    serde_json::from_slice::<Claims>(&payload).ok()
}

/// O access_token é um JWT — o claim `email` já vem embutido nele
/// (assinado pelo GoTrue no momento em que o Google devolveu o login), só
/// decodificar a parte do meio em base64. Evita uma chamada de rede a
/// mais depois do deep link (que podia falhar — proxy, DNS, o que for —
/// sem aparecer erro nenhum pro jogador, só deixando "Conectado como"
/// em branco).
fn decode_email_from_jwt(access_token: &str) -> Option<String> {
    claims(access_token)?.email
}

/// Quantos segundos faltam pra chave de acesso vencer (pelo claim `exp`).
/// `None` quando não dá pra ler — quem chama trata como vencida e renova.
pub fn seconds_until_expiry(access_token: &str) -> Option<i64> {
    let exp = claims(access_token)?.exp?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(exp - now)
}

#[derive(Debug, Deserialize)]
struct ExchangeCodeResponse {
    access_token: String,
    refresh_token: String,
}

/// Troca o código de uso único do login do agente (gerado em
/// `app/auth/confirm` no produto quando o login com Google volta pro
/// agente) pelos tokens de sessão reais. O deep link
/// `radar-pokersync://auth?code=...&state=...` só traz esse código —
/// nunca mais os tokens direto, que antes trafegavam pela URL/tela de
/// "copiar link" (ver `app/agent-login/concluido` no produto e
/// `app/api/agent/exchange-code/route.ts`). O código é de uso único e
/// expira em minutos, então mesmo exposto (histórico do navegador,
/// logs) não vale nada depois de trocado ou vencido.
pub async fn exchange_login_code(base_url: &str, code: &str) -> Result<LoginResult, String> {
    let client = sync_client::http_client();
    let url = format!("{base_url}/api/agent/exchange-code");
    let resp = client
        .post(url)
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .map_err(|_| SEM_CONEXAO.to_string())?;

    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|_| SEM_CONEXAO.to_string())?;
    if !status.is_success() {
        return Err("Código de login inválido ou expirado — tente entrar de novo.".to_string());
    }

    let parsed: ExchangeCodeResponse = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let email = fetch_user_email(&parsed.access_token).await;
    Ok(LoginResult {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        email,
    })
}

/// Busca o email do usuário a partir do access_token — usada pelo login
/// com Google (`exchange_login_code` acima). Tenta primeiro decodificar
/// do próprio token (rápido, sem rede); só bate no GoTrue se por algum
/// motivo o token não tiver o claim (não deveria acontecer com os tokens
/// que o Supabase emite hoje, mas mais vale ter o caminho de volta).
pub async fn fetch_user_email(access_token: &str) -> Option<String> {
    if let Some(email) = decode_email_from_jwt(access_token) {
        return Some(email);
    }

    let client = sync_client::http_client();
    let url = format!("{}/auth/v1/user", supabase_url());
    let resp = client
        .get(url)
        .header("apikey", SUPABASE_ANON_KEY)
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<GoTrueUser>().await.ok()?.email
}

#[cfg(test)]
mod tests {
    use super::{decode_email_from_jwt, seconds_until_expiry, traduzir_erro_login};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    fn fake_jwt(payload_json: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.fake-signature")
    }

    #[test]
    fn decodes_email_from_real_shaped_token() {
        let token = fake_jwt(r#"{"sub":"123","email":"jogador@pokersync.com.br","role":"authenticated"}"#);
        assert_eq!(decode_email_from_jwt(&token).as_deref(), Some("jogador@pokersync.com.br"));
    }

    #[test]
    fn returns_none_when_claim_missing() {
        let token = fake_jwt(r#"{"sub":"123","role":"authenticated"}"#);
        assert_eq!(decode_email_from_jwt(&token), None);
    }

    #[test]
    fn returns_none_for_garbage_input() {
        assert_eq!(decode_email_from_jwt("nao-e-um-jwt"), None);
        assert_eq!(seconds_until_expiry("nao-e-um-jwt"), None);
    }

    #[test]
    fn reads_time_left_from_exp_claim() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let token = fake_jwt(&format!(r#"{{"sub":"123","exp":{}}}"#, now + 600));
        let left = seconds_until_expiry(&token).unwrap();
        assert!((595..=600).contains(&left), "sobrou {left}");

        let vencido = fake_jwt(&format!(r#"{{"sub":"123","exp":{}}}"#, now - 5));
        assert!(seconds_until_expiry(&vencido).unwrap() < 0);
    }

    #[test]
    fn translates_common_login_errors() {
        assert_eq!(traduzir_erro_login("Invalid login credentials"), "Email ou senha inválidos.");
        assert!(traduzir_erro_login("Email not confirmed").starts_with("Confirme seu email"));
        assert_eq!(traduzir_erro_login("Outra coisa"), "Outra coisa");
    }
}
