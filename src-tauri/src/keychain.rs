//! Chave de renovação da sessão guardada no keychain nativo do SO —
//! Windows Credential Manager, macOS Keychain, Secret Service no Linux —
//! em vez de arquivo texto plano. O resto da config (device, pastas) não é
//! segredo e continua em `config.rs`.
//!
//! Desde a 0.2.0 só a chave de RENOVAÇÃO fica aqui. A chave de acesso (um
//! JWT grande, que vence em ~1 hora) vive só na memória do app e é
//! renovada ao abrir. Antes as duas iam juntas num JSON que passava do
//! limite do Windows Credential Manager (2560 bytes em UTF-16) e precisava
//! ser picado em vários pedaços — gravar vários pedaços não é atômico, e
//! um desligamento no meio da gravação deixava o login corrompido. A chave
//! de renovação é curta e cabe numa entrada só.
//!
//! As funções de leitura dos formatos antigos continuam aqui só pra quem
//! atualiza da 0.1.x não precisar entrar de novo.

use keyring::Entry;
use serde::Deserialize;

const SERVICE: &str = "com.pokersync.radar";
const ACCOUNT_REFRESH: &str = "refresh-token";

/// Formatos antigos (0.1.x): JSON {access_token, refresh_token} em pedaços
/// ("session-chunk-N" + "session-chunk-count"), numa entrada só
/// ("session") ou no serviço de antes do rename pra "Radar PokerSync".
const SERVICE_LEGACY: &str = "com.pokersync.agent";
const ACCOUNT_LEGACY: &str = "session";
const ACCOUNT_COUNT: &str = "session-chunk-count";
/// Trava contra uma contagem de pedaços corrompida.
const MAX_CHUNKS: usize = 50;

#[derive(Debug, Deserialize)]
struct LegacyTokens {
    refresh_token: String,
}

fn entry(account: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, account).map_err(|e| format!("Keychain indisponível: {e}"))
}

fn legacy_entry() -> Result<Entry, String> {
    Entry::new(SERVICE_LEGACY, ACCOUNT_LEGACY).map_err(|e| format!("Keychain indisponível: {e}"))
}

fn chunk_account(index: usize) -> String {
    format!("session-chunk-{index}")
}

fn read_chunk_count() -> Option<usize> {
    let raw = entry(ACCOUNT_COUNT).ok()?.get_password().ok()?;
    raw.parse::<usize>().ok().filter(|n| *n <= MAX_CHUNKS)
}

fn load_chunked() -> Option<String> {
    let count = read_chunk_count()?;
    let mut raw = String::new();
    for i in 0..count {
        raw.push_str(&entry(&chunk_account(i)).ok()?.get_password().ok()?);
    }
    Some(raw)
}

fn load_legacy_single() -> Option<String> {
    let same_service = entry(ACCOUNT_LEGACY).ok().and_then(|e| e.get_password().ok());
    same_service.or_else(|| legacy_entry().ok().and_then(|e| e.get_password().ok()))
}

fn refresh_from_legacy_json(raw: &str) -> Option<String> {
    serde_json::from_str::<LegacyTokens>(raw).ok().map(|t| t.refresh_token)
}

/// `None` tanto quando não há sessão salva quanto quando o backend do
/// keychain falha (ex.: ambiente sem Secret Service no Linux) — nesse caso
/// o app trata como "não logado" e pede login de novo, em vez de travar.
pub fn load_refresh_token() -> Option<String> {
    if let Some(token) = entry(ACCOUNT_REFRESH).ok().and_then(|e| e.get_password().ok()) {
        if !token.is_empty() {
            return Some(token);
        }
    }
    load_chunked()
        .and_then(|raw| refresh_from_legacy_json(&raw))
        .or_else(|| load_legacy_single().and_then(|raw| refresh_from_legacy_json(&raw)))
}

fn delete(entry: Result<Entry, String>) -> Result<(), String> {
    match entry?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn delete_legacy() {
    let count = read_chunk_count().unwrap_or(MAX_CHUNKS.min(8));
    for i in 0..count {
        let _ = delete(entry(&chunk_account(i)));
    }
    let _ = delete(entry(ACCOUNT_COUNT));
    let _ = delete(entry(ACCOUNT_LEGACY));
    let _ = delete(legacy_entry());
}

/// Uma gravação só (atômica do ponto de vista do app). Depois de gravar,
/// apaga os formatos antigos pra não deixar chave velha parada no cofre.
pub fn save_refresh_token(refresh_token: &str) -> Result<(), String> {
    entry(ACCOUNT_REFRESH)?
        .set_password(refresh_token)
        .map_err(|e| e.to_string())?;
    delete_legacy();
    Ok(())
}

pub fn clear() -> Result<(), String> {
    delete_legacy();
    delete(entry(ACCOUNT_REFRESH))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Limite real do Windows Credential Manager, em bytes de UTF-16.
    const CRED_MAX_CREDENTIAL_BLOB_SIZE_BYTES: usize = 2560;

    #[test]
    fn refresh_tokens_fit_in_a_single_windows_entry() {
        // O Supabase emite chaves de renovação curtas (dezenas de
        // caracteres); 500 já é uma folga enorme e ainda cabe.
        let token = "r".repeat(500);
        assert!(token.encode_utf16().count() * 2 <= CRED_MAX_CREDENTIAL_BLOB_SIZE_BYTES);
    }

    #[test]
    fn reads_refresh_token_from_legacy_json() {
        let raw = r#"{"access_token":"a.b.c","refresh_token":"abc123"}"#;
        assert_eq!(refresh_from_legacy_json(raw).as_deref(), Some("abc123"));
        assert_eq!(refresh_from_legacy_json("lixo"), None);
    }
}
