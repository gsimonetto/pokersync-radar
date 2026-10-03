//! Memória de "de qual sala é cada arquivo". O ciclo automático varre as
//! pastas a cada 5 minutos; antes, abria de novo todos os arquivos de
//! anos de histórico (uma vez pra cada sala) só pra reconhecer o que já
//! tinha reconhecido. Agora só abre arquivo novo ou que mudou.

use crate::room::PokerRoom;
use crate::state::FileSignature;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Sobe quando as regras de reconhecimento (`PokerRoom::classify`) mudam:
/// a memória antiga é jogada fora e todo arquivo é reconhecido de novo
/// pelas regras novas (ex.: arquivos do ACR que antes eram ignorados).
/// 2: mão real do ACR ("Game Hand #") e resumo .ots (JSON) — 03/10/2026.
pub const CLASSIFIER_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    signature: FileSignature,
    /// `None` = arquivo que nenhuma sala reconhece (também vale lembrar:
    /// não precisa ser aberto de novo enquanto não mudar).
    room: Option<PokerRoom>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClassCache {
    version: u32,
    entries: HashMap<PathBuf, Entry>,
    #[serde(skip)]
    seen: HashSet<PathBuf>,
    #[serde(skip)]
    dirty: bool,
}

/// Memória vazia (versão atual) — também o resultado de um arquivo que
/// não existe, está corrompido ou é de outra versão das regras.
impl Default for ClassCache {
    fn default() -> Self {
        ClassCache {
            version: CLASSIFIER_VERSION,
            entries: HashMap::new(),
            seen: HashSet::new(),
            dirty: false,
        }
    }
}

impl ClassCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<ClassCache>(&s).ok())
            .filter(|c| c.version == CLASSIFIER_VERSION)
            .unwrap_or_default()
    }

    /// Grava só se algo mudou, esquecendo antes os arquivos que não
    /// apareceram nesta varredura (apagados ou movidos).
    pub fn save_if_changed(&mut self, path: &Path) -> std::io::Result<()> {
        let before = self.entries.len();
        let seen = std::mem::take(&mut self.seen);
        self.entries.retain(|p, _| seen.contains(p));
        if !self.dirty && self.entries.len() == before {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

    /// A sala já reconhecida pra esse arquivo, se ele não mudou desde então.
    /// `Some(None)` = já se sabe que nenhuma sala reconhece.
    pub(crate) fn get(&mut self, path: &Path, signature: FileSignature) -> Option<Option<PokerRoom>> {
        self.seen.insert(path.to_path_buf());
        self.entries
            .get(path)
            .filter(|e| e.signature == signature)
            .map(|e| e.room)
    }

    pub(crate) fn put(&mut self, path: &Path, signature: FileSignature, room: Option<PokerRoom>) {
        self.seen.insert(path.to_path_buf());
        self.entries.insert(path.to_path_buf(), Entry { signature, room });
        self.dirty = true;
    }
}
