//! Cliente HTTP do agente pra falar com app/api/agent/{ping,sync}. Os nomes
//! de campo (camelCase) e o formato de resposta espelham exatamente
//! lib/services/agent-sync-service.ts + app/api/agent/sync/route.ts — os
//! dois lados devem mudar juntos.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_BATCH_SIZE: usize = 50;

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
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("erro de rede: {0}")]
    Network(#[from] reqwest::Error),
    #[error("resposta inesperada do servidor: {0}")]
    BadResponse(String),
    #[error("servidor recusou (status {status}): {message}")]
    Rejected { status: u16, message: String },
}

pub struct SyncClient {
    http: reqwest::Client,
    base_url: String,
    access_token: String,
}

impl SyncClient {
    pub fn new(base_url: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            access_token: access_token.into(),
        }
    }

    /// Valida token + conectividade (usado pelo botão "Testar conexão").
    pub async fn ping(&self) -> Result<(), SyncError> {
        let url = format!("{}/api/agent/ping", self.base_url);
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.access_token)
            .send()
            .await?;
        Self::body_or_err(resp).await.map(|_| ())
    }

    /// Envia um lote de arquivos de uma sala. O chamador é responsável por
    /// dividir listas grandes (ver `chunk_files`) — o backend limita 200
    /// arquivos por request.
    pub async fn sync_batch(
        &self,
        device: &DeviceInfo,
        poker_room: &str,
        files: &[SyncFile],
    ) -> Result<SyncBatchResult, SyncError> {
        let url = format!("{}/api/agent/sync", self.base_url);
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
        let value = Self::body_or_err(resp).await?;
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
        let url = format!("{}/api/agent/sync-tournaments", self.base_url);
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
        let value = Self::body_or_err(resp).await?;
        serde_json::from_value(value).map_err(|e| SyncError::BadResponse(e.to_string()))
    }

    async fn body_or_err(resp: reqwest::Response) -> Result<serde_json::Value, SyncError> {
        let status = resp.status();
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SyncError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            // O backend manda "error" (código curto, ex.: "IMPORT_SCOPE_NAO_DEFINIDO")
            // e, em alguns casos, um "message" com o texto amigável pro usuário —
            // prefere o "message" quando existir, senão cai pro "error".
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("error").and_then(|v| v.as_str()))
                .unwrap_or("erro desconhecido")
                .to_string();
            return Err(SyncError::Rejected {
                status: status.as_u16(),
                message,
            });
        }
        Ok(value)
    }
}

/// Quebra `files` em lotes de até `batch_size` — mantém o request dentro do
/// limite do backend e permite reportar progresso incremental na UI.
pub fn chunk_files(files: Vec<SyncFile>, batch_size: usize) -> Vec<Vec<SyncFile>> {
    let batch_size = batch_size.max(1);
    let mut out = Vec::new();
    let mut current = Vec::new();
    for f in files {
        current.push(f);
        if current.len() >= batch_size {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
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

    #[tokio::test]
    async fn ping_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/agent/ping"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let client = SyncClient::new(server.uri(), "tok");
        client.ping().await.unwrap();
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
        let err = client.ping().await.unwrap_err();
        match err {
            SyncError::Rejected { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "Token inválido.");
            }
            other => panic!("esperava Rejected, veio {other:?}"),
        }
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
    }

    #[test]
    fn chunk_files_splits_by_batch_size() {
        let files: Vec<SyncFile> = (0..5)
            .map(|i| SyncFile {
                raw_text: format!("hand {i}"),
                captured_at: None,
            })
            .collect();
        let batches = chunk_files(files, 2);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[2].len(), 1);
    }
}
