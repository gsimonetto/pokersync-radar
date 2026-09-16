//! Config local do agente: URL do produto, device e pastas extras por sala.
//! Persistida em JSON em app_config_dir() (por usuário/SO, via
//! tauri::Manager::path). Os tokens de sessão NÃO ficam aqui — vão pro
//! keychain do SO (ver `keychain.rs`), porque isto é um arquivo texto
//! plano e tokens são segredo.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// Mesmos valores públicos usados pelo client web (lib/supabase/client.ts) —
// a anon key é destinada a ficar em qualquer bundle de cliente, RLS é quem
// protege os dados.
pub const SUPABASE_URL: &str = "https://olgziujndtlvxegcnaoq.supabase.co";
pub const SUPABASE_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Im9sZ3ppdWpuZHRsdnhlZ2NuYW9xIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODUxNjExMDYsImV4cCI6MjEwMDczNzEwNn0.NspOVPJcZ_pjodnDTHzalDIcjVkqoR6YVfwiN4MpBbY";

/// Domínio de produção do PokerSync — fixo, sem campo editável em lugar
/// nenhum da UI (não existe mais "outro site" pra digitar; era pensado
/// só pra depuração interna e nunca deveria ter ficado exposto pro
/// jogador). Todo lugar que precisa da URL do produto (login com
/// Google, troca do código de login, sincronização, teste de conexão)
/// usa esta constante direto.
pub const DEFAULT_BASE_URL: &str = "https://www.pokersync.com.br";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub user_email: Option<String>,
    /// Identifica esta instalação em hand_sync_devices.device_id — gerado
    /// uma vez e reaproveitado entre sessões do app.
    #[serde(default = "new_device_id")]
    pub device_id: String,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    /// Pastas adicionais escolhidas manualmente pelo usuário, por TIPO de
    /// arquivo (slug de FileKind: "hands"/"tournaments") — somam-se aos
    /// caminhos padrão do SO, varridas em todas as salas de uma vez (a UI
    /// não pede mais "escolha a sala" — ela descobre sozinha, por sniff,
    /// de qual sala cada arquivo é). Antes era por sala; virou por tipo
    /// quando a tela de import deixou de ser "uma sala por vez" e passou a
    /// ser só "mãos" ou "torneios" (2026-09).
    #[serde(default)]
    pub extra_folders: HashMap<String, Vec<String>>,

    /// Liga a sincronização periódica em background (padrão: ligado) — sem
    /// isso o jogador precisava lembrar de clicar em "Sincronizar agora"
    /// depois de cada sessão. Ver `spawn_auto_sync` em lib.rs.
    #[serde(default = "default_true")]
    pub auto_sync_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn new_device_id() -> String {
    format!("agent-{}", uuid::Uuid::new_v4())
}

fn default_device_name() -> String {
    hostname()
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PC desconhecido".to_string())
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            user_email: None,
            device_id: new_device_id(),
            device_name: default_device_name(),
            extra_folders: HashMap::new(),
            auto_sync_enabled: true,
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }
}
