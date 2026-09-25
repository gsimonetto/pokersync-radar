//! Cliente HTTP do agente pra falar com app/api/agent/{ping,sync,sync-tournaments}.
//! Os nomes de campo (camelCase) e o formato de resposta espelham
//! lib/services/agent-sync-service.ts + app/api/agent/*/route.ts no
//! produto — os dois lados devem mudar juntos.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Limites de envio. O servidor do PokerSync (Vercel) recusa requisição com
/// corpo acima de ~4,5 MB, e as rotas aceitam até 200 arquivos por envio,
/// 5 MB por arquivo de mãos e 1 MB por resumo de torneio. Antes os lotes
/// eram só "50 arquivos", sem olhar tamanho — num histórico grande o envio
/// era recusado e aquela sala ficava travada pra sempre, sem aviso.
pub const MAX_FILES_PER_BATCH: usize = 50;
pub const MAX_BATCH_BYTES: usize = 3_000_000;
/// Arquivo de mãos maior que isso vai em partes (cortadas no começo de uma
/// mão, ver `scanner::text::split_into_parts`).
pub const MAX_HAND_PART_BYTES: usize = 2_000_000;
/// Resumo de torneio não pode ser partido (o backend lê o arquivo inteiro);
/// acima disso não é um resumo de verdade e fica de fora.
pub const MAX_SUMMARY_BYTES: usize = 900_000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Folgado pra um lote de ~3 MB numa conexão lenta — mas nunca "pra sempre":
/// sem isso, uma conexão que engasgava no meio deixava o Radar
/// "sincronizando" indefinidamente.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Cliente HTTP com tempo limite — usado também pelo login (src-tauri/auth.rs).
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    #[serde(rename = "deviceName")]
    pub device_name: String,
    pub platform: String,
    #[serde(rename = "agentVersion")]
    pub agent_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncFile {
    #[serde(rename = "rawText")]
    pub raw_text: String,
    #[serde(rename = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SyncRequest<'a> {
    device: &'a DeviceInfo,
    #[serde(rename = "pokerRoom")]
    poker_room: &'a str,
    files: &'a [SyncFile],
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SyncBatchResult {
    #[serde(rename = "batchId")]
    pub batch_id: String,
    #[serde(rename = "totalHands")]
    pub total_hands: u32,
    pub imported: u32,
    pub duplicates: u32,
    pub errors: u32,
    /// Mãos jogadas antes do corte "só a partir de agora" (ver
    /// profiles.radar_import_scope_since no produto). Ausente em versões do
    /// site anteriores a esse corte.
    #[serde(default, rename = "ignoradasPorData")]
    pub skipped_by_date: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TournamentSyncBatchResult {
    #[serde(rename = "batchId")]
    pub batch_id: String,
    #[serde(rename = "totalFiles")]
    pub total_files: u32,
    pub imported: u32,
    pub duplicates: u32,
    pub errors: u32,
    #[serde(default, rename = "ignoradasPorData")]
    pub skipped_by_date: u32,
}

/// O que o site responde no "sinal de vida" (`/api/agent/ping`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PingInfo {
    #[serde(default)]
    pub email: Option<String>,
    /// `None` = o jogador ainda não escolheu, na tela do Radar no site, o
    /// que importar — os envios são recusados até lá.
    #[serde(default, rename = "importScope")]
    pub import_scope: Option<String>,
    /// `Some(false)` = o plano da conta não inclui o Radar. Ausente em
    /// versões do site anteriores a essa checagem.
    #[serde(default, rename = "radarLiberado")]
    pub radar_liberado: Option<bool>,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("Sem conexão com o PokerSync — confira a internet. O Radar tenta de novo sozinho.")]
    Network(#[source] reqwest::Error),
    #[error("Resposta inesperada do PokerSync: {0}")]
    BadResponse(String),
    /// `code` é o código curto do backend (ex.: "IMPORT_SCOPE_NAO_DEFINIDO",
    /// "RADAR_FORA_DO_PLANO"); `message` é o texto pro jogador.
    #[error("{message}")]
    Rejected {
        status: u16,
        code: Option<String>,
        message: String,
    },
}

impl From<reqwest::Error> for SyncError {
    fn from(e: reqwest::Error) -> Self {
        SyncError::Network(e)
    }
}

impl SyncError {
    pub fn status(&self) -> Option<u16> {
        match self {
            SyncError::Rejected { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            SyncError::Rejected { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Falha passageira (rede, servidor instável, excesso de tentativas) —
    /// vale tentar de novo daqui a pouco, sem mexer no login.
    pub fn is_transient(&self) -> bool {
        match self {
            SyncError::Network(_) => true,
            SyncError::Rejected { status, .. } => *status == 429 || *status >= 500,
            SyncError::BadResponse(_) => false,
        }
    }
}

/// Texto pro jogador quando o servidor não mandou mensagem própria (ex.:
/// página de erro em HTML da Vercel).
fn message_for_status(status: u16) -> String {
    match status {
        401 => "Sessão expirada — entre de novo.".to_string(),
        403 => "O PokerSync recusou o acesso desta conta.".to_string(),
        413 => "Envio grande demais para o PokerSync.".to_string(),
        429 => "Muitas tentativas seguidas — o Radar espera um pouco e tenta de novo.".to_string(),
        500..=599 => "O PokerSync está instável agora — o Radar tenta de novo sozinho.".to_string(),
        _ => format!("O PokerSync recusou o envio (código {status})."),
    }
}

pub struct SyncClient {
    http: reqwest::Client,
    base_url: String,
    access_token: String,
}

impl SyncClient {
    pub fn new(base_url: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self {
            http: http_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            access_token: access_token.into(),
        }
    }

    /// Valida a sessão e manda o "sinal de vida" deste computador (com
    /// `device`, o site guarda quando o Radar foi visto pela última vez).
    /// Também traz o que o jogador escolheu importar e se o plano inclui o
    /// Radar.
    pub async fn ping(&self, device: Option<&DeviceInfo>) -> Result<PingInfo, SyncError> {
        let url = format!("{}/api/agent/ping", self.base_url);
        let attempt = match device {
            Some(device) => {
                let resp = self
                    .http
                    .post(&url)
                    .bearer_auth(&self.access_token)
                    .json(&serde_json::json!({ "device": device }))
                    .send()
                    .await?;
                Self::body_or_err(resp).await
            }
            None => self.ping_get(&url).await,
        };
        let value = match attempt {
            // Site anterior ao "sinal de vida" só responde GET.
            Err(SyncError::Rejected { status: 405, .. }) if device.is_some() => self.ping_get(&url).await?,
            other => other?,
        };
        serde_json::from_value(value).map_err(|e| SyncError::BadResponse(e.to_string()))
    }

    /// Salva o que o jogador escolheu importar ("from_now",
    /// "last_3_months" ou "full_history") — a mesma escolha da tela do
    /// Radar no site (`/api/agent/import-scope`).
    pub async fn set_import_scope(&self, scope: &str) -> Result<(), SyncError> {
        let url = format!("{}/api/agent/import-scope", self.base_url);
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "scope": scope }))
            .send()
            .await?;
        Self::body_or_err(resp).await.map(|_| ())
    }

    async fn ping_get(&self, url: &str) -> Result<serde_json::Value, SyncError> {
        let resp = self.http.get(url).bearer_auth(&self.access_token).send().await?;
        Self::body_or_err(resp).await
    }

    /// Envia um lote de arquivos de mãos de uma sala. Quem chama monta o
    /// lote dentro dos limites (ver `BatchBuilder`).
    pub async fn sync_batch(
        &self,
        device: &DeviceInfo,
        poker_room: &str,
        files: &[SyncFile],
    ) -> Result<SyncBatchResult, SyncError> {
        let value = self.post_files("sync", device, poker_room, files).await?;
        serde_json::from_value(value).map_err(|e| SyncError::BadResponse(e.to_string()))
    }

    /// Mesmo formato de `sync_batch`, mas pro resumo de torneio — endpoint
    /// separado (`/api/agent/sync-tournaments`) porque o backend faz outra
    /// coisa com o texto (extrai buy-in/colocação/premiação em vez de
    /// mãos, ver `lib/poker/tournament-summary-parser.ts` no produto).
    pub async fn sync_tournament_batch(
        &self,
        device: &DeviceInfo,
        poker_room: &str,
        files: &[SyncFile],
    ) -> Result<TournamentSyncBatchResult, SyncError> {
        let value = self.post_files("sync-tournaments", device, poker_room, files).await?;
        serde_json::from_value(value).map_err(|e| SyncError::BadResponse(e.to_string()))
    }

    async fn post_files(
        &self,
        route: &str,
        device: &DeviceInfo,
        poker_room: &str,
        files: &[SyncFile],
    ) -> Result<serde_json::Value, SyncError> {
        let url = format!("{}/api/agent/{route}", self.base_url);
        let body = SyncRequest {
            device,
            poker_room,
            files,
        };
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await?;
        Self::body_or_err(resp).await
    }

    /// Olha o status ANTES de tentar ler JSON: página de erro em HTML (ex.:
    /// envio grande demais, servidor fora) virava "error decoding response
    /// body" em vez de uma mensagem que o jogador entende.
    async fn body_or_err(resp: reqwest::Response) -> Result<serde_json::Value, SyncError> {
        let status = resp.status();
        let bytes = resp.bytes().await?;
        let value: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
        if !status.is_success() {
            // O backend manda "error" (código curto, ex.: "IMPORT_SCOPE_NAO_DEFINIDO")
            // e, em alguns casos, um "message" com o texto amigável — prefere
            // o "message" quando existir, senão o "error", senão um texto
            // pelo status.
            let field = |name: &str| {
                value
                    .as_ref()
                    .and_then(|v| v.get(name))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            let code = field("error");
            let message = field("message")
                .or_else(|| code.clone())
                .unwrap_or_else(|| message_for_status(status.as_u16()));
            return Err(SyncError::Rejected {
                status: status.as_u16(),
                code,
                message,
            });
        }
        value.ok_or_else(|| SyncError::BadResponse("resposta sem JSON".to_string()))
    }
}

/// Monta lotes de envio respeitando os dois limites ao mesmo tempo:
/// quantidade de arquivos e tamanho total. Um item maior que o limite de
/// bytes vai sozinho num lote (quem chama já parte arquivos grandes antes).
pub struct BatchBuilder {
    max_items: usize,
    max_bytes: usize,
    items: Vec<SyncFile>,
    bytes: usize,
}

impl BatchBuilder {
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
            items: Vec::new(),
            bytes: 0,
        }
    }

    /// `true` quando um item de `item_bytes` não cabe mais no lote atual —
    /// quem chama envia o lote (`take`) antes de adicionar o item.
    pub fn is_full_for(&self, item_bytes: usize) -> bool {
        !self.items.is_empty() && (self.items.len() >= self.max_items || self.bytes + item_bytes > self.max_bytes)
    }

    pub fn push(&mut self, file: SyncFile) {
        self.bytes += file.raw_text.len();
        self.items.push(file);
    }

    pub fn take(&mut self) -> Vec<SyncFile> {
        self.bytes = 0;
        std::mem::take(&mut self.items)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn device() -> DeviceInfo {
        DeviceInfo {
            device_id: "dev-1".into(),
            device_name: "Meu PC".into(),
            platform: "linux".into(),
            agent_version: "0.1.0".into(),
        }
    }

    fn file(text: &str) -> SyncFile {
        SyncFile {
            raw_text: text.into(),
            captured_at: None,
        }
    }

    #[tokio::test]
    async fn ping_sends_heartbeat_and_reads_scope_and_plan() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/ping"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({
                "device": {"deviceId": "dev-1", "deviceName": "Meu PC", "platform": "linux", "agentVersion": "0.1.0"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "email": "a@b.com", "importScope": "from_now", "radarLiberado": false
            })))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let info = client.ping(Some(&device())).await.unwrap();
        assert_eq!(info.import_scope.as_deref(), Some("from_now"));
        assert_eq!(info.radar_liberado, Some(false));
    }

    #[tokio::test]
    async fn ping_falls_back_to_get_on_old_site() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/ping"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/agent/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "importScope": null})))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let info = client.ping(Some(&device())).await.unwrap();
        assert_eq!(info.import_scope, None);
        assert_eq!(info.radar_liberado, None);
    }

    #[tokio::test]
    async fn ping_surfaces_server_error_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/agent/ping"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"ok": false, "error": "Token inválido."})),
            )
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "bad-tok");
        let err = client.ping(None).await.unwrap_err();
        assert_eq!(err.status(), Some(401));
        assert_eq!(err.to_string(), "Token inválido.");
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn html_error_page_becomes_friendly_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/sync"))
            .respond_with(ResponseTemplate::new(413).set_body_string("<html>Request Entity Too Large</html>"))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let err = client.sync_batch(&device(), "pokerstars", &[file("x")]).await.unwrap_err();
        assert_eq!(err.status(), Some(413));
        assert_eq!(err.to_string(), "Envio grande demais para o PokerSync.");
    }

    #[tokio::test]
    async fn keeps_backend_code_and_friendly_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/sync"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "ok": false, "error": "RADAR_FORA_DO_PLANO", "message": "Seu plano não inclui o Radar."
            })))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let err = client.sync_batch(&device(), "pokerstars", &[file("x")]).await.unwrap_err();
        assert_eq!(err.code(), Some("RADAR_FORA_DO_PLANO"));
        assert_eq!(err.to_string(), "Seu plano não inclui o Radar.");
    }

    #[tokio::test]
    async fn server_errors_are_transient() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/sync-tournaments"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let err = client
            .sync_tournament_batch(&device(), "pokerstars", &[file("x")])
            .await
            .unwrap_err();
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn sync_batch_sends_expected_payload_and_parses_result() {
        let server = MockServer::start().await;
        let files = vec![SyncFile {
            raw_text: "PokerStars Hand #1: ...".into(),
            captured_at: Some("2026-08-01T00:00:00Z".into()),
        }];
        let expected_body = serde_json::json!({
            "device": {"deviceId": "dev-1", "deviceName": "Meu PC", "platform": "linux", "agentVersion": "0.1.0"},
            "pokerRoom": "pokerstars",
            "files": [{"rawText": "PokerStars Hand #1: ...", "capturedAt": "2026-08-01T00:00:00Z"}],
        });

        Mock::given(method("POST"))
            .and(path("/api/agent/sync"))
            .and(body_json(&expected_body))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "batchId": "batch-1",
                "totalHands": 1,
                "imported": 1,
                "duplicates": 0,
                "errors": 0
            })))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        let result = client
            .sync_batch(&device(), "pokerstars", &files)
            .await
            .unwrap();
        assert_eq!(result.batch_id, "batch-1");
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped_by_date, 0);
    }

    #[tokio::test]
    async fn saves_import_choice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/agent/import-scope"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({ "scope": "last_3_months" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "importScope": "last_3_months"})))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        client.set_import_scope("last_3_months").await.unwrap();
    }

    #[test]
    fn batch_builder_respects_count_and_size() {
        let mut b = BatchBuilder::new(2, 10);
        assert!(!b.is_full_for(100)); // lote vazio sempre aceita (item grande vai sozinho)
        b.push(file("12345"));
        assert!(!b.is_full_for(5));
        assert!(b.is_full_for(6)); // passaria de 10 bytes
        b.push(file("12345"));
        assert!(b.is_full_for(0)); // chegou em 2 itens
        assert_eq!(b.take().len(), 2);
        assert!(b.is_empty());
        assert!(!b.is_full_for(10));
    }
}
