use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Assinatura de um arquivo já sincronizado: tamanho + mtime (epoch
/// segundos). Suficiente pra detectar "arquivo cresceu / mudou" sem
/// precisar reler+hashear tudo a cada scan — hand history só cresce
/// (append-only pelos clientes de poker), então isso raramente dá falso
/// negativo na prática.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSignature {
    pub size: u64,
    pub modified_unix: i64,
}

/// Quanto do texto (já decodificado) de um arquivo foi enviado, e a
/// impressão digital desse trecho — permite mandar só o que foi anexado
/// depois (ver `text::new_part_since`), em vez do arquivo inteiro de novo a
/// cada varredura enquanto uma sessão está rolando.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentText {
    pub len: u64,
    pub fingerprint: u64,
}

/// Quantas vezes seguidas o site pode recusar um arquivo (enviado sozinho)
/// com erro do servidor antes de o Radar deixá-lo de lado e seguir com os
/// outros. Recusa por conteúdo (erro 4xx) deixa de lado na primeira.
pub const MAX_FAILED_ATTEMPTS: u32 = 3;
/// Arquivo deixado de lado é tentado de novo depois disso (ou na hora, se
/// mudar) — o problema pode ter sido do servidor, e já ter sido corrigido.
pub const RETRY_GIVEN_UP_AFTER_SECS: i64 = 24 * 60 * 60;

/// Arquivo que o site recusou mesmo enviado sozinho.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedFile {
    pub signature: FileSignature,
    pub attempts: u32,
    /// Quando o Radar desistiu de enviar (segundos desde 1970).
    pub given_up_at: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    synced_files: HashMap<PathBuf, FileSignature>,
    /// Ausente em arquivos de estado antigos (antes da 0.2.0) — nesse caso o
    /// próximo envio de um arquivo que cresceu vai inteiro, como antes.
    #[serde(default)]
    sent_text: HashMap<PathBuf, SentText>,
    /// Arquivos que o site recusou. Antes, um arquivo recusado sempre
    /// travava o envio ali: o Radar tentava o mesmo lote pra sempre e nada
    /// depois dele (nem as outras salas) chegava ao site.
    #[serde(default)]
    failed: HashMap<PathBuf, FailedFile>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SyncState {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        // Grava num arquivo ao lado e troca de uma vez: se o PC desligar no
        // meio, fica o estado anterior inteiro (no pior caso reenvia algo
        // que o servidor descarta como repetido), nunca um arquivo pela
        // metade que zeraria o progresso de tudo.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }

    /// Um arquivo precisa ser (re)sincronizado se nunca foi visto antes, ou
    /// se mudou de tamanho/mtime desde o último sync (ex.: novas mãos
    /// anexadas no fim do arquivo do dia).
    /// Arquivo deixado de lado (ver `record_failure`) não conta enquanto
    /// não mudar e não passar `RETRY_GIVEN_UP_AFTER_SECS`.
    pub fn needs_sync(&self, path: &Path, sig: FileSignature) -> bool {
        self.needs_sync_at(path, sig, now_unix())
    }

    pub fn needs_sync_at(&self, path: &Path, sig: FileSignature, now: i64) -> bool {
        if self.synced_files.get(path) == Some(&sig) {
            return false;
        }
        match self.failed.get(path) {
            Some(f) if f.signature == sig => {
                !matches!(f.given_up_at, Some(t) if now - t < RETRY_GIVEN_UP_AFTER_SECS)
            }
            _ => true,
        }
    }

    pub fn mark_synced(&mut self, path: PathBuf, sig: FileSignature) {
        self.failed.remove(&path);
        self.synced_files.insert(path, sig);
    }

    /// Igual a `mark_synced`, guardando também quanto do texto foi enviado.
    pub fn mark_synced_text(&mut self, path: PathBuf, sig: FileSignature, sent: SentText) {
        self.failed.remove(&path);
        self.sent_text.insert(path.clone(), sent);
        self.synced_files.insert(path, sig);
    }

    /// O site recusou este arquivo, enviado sozinho. Conta a tentativa e
    /// devolve `true` se o Radar deve deixá-lo de lado e seguir com os
    /// outros: de cara quando `by_content` (recusa pelo conteúdo, que não
    /// muda tentando de novo), ou na `MAX_FAILED_ATTEMPTS`-ésima recusa
    /// seguida. Se o arquivo mudou desde a última recusa, a conta recomeça.
    pub fn record_failure(&mut self, path: PathBuf, sig: FileSignature, by_content: bool, now: i64) -> bool {
        let entry = self.failed.entry(path).or_insert(FailedFile {
            signature: sig,
            attempts: 0,
            given_up_at: None,
        });
        if entry.signature != sig {
            *entry = FailedFile {
                signature: sig,
                attempts: 0,
                given_up_at: None,
            };
        }
        entry.attempts += 1;
        let give_up = by_content || entry.attempts >= MAX_FAILED_ATTEMPTS;
        if give_up {
            entry.given_up_at = Some(now);
        }
        give_up
    }

    /// Quantos arquivos o Radar deixou de lado (recusados pelo site).
    pub fn given_up_count(&self) -> usize {
        self.failed.values().filter(|f| f.given_up_at.is_some()).count()
    }

    pub fn sent_text(&self, path: &Path) -> Option<SentText> {
        self.sent_text.get(path).copied()
    }
}

pub fn signature_of(path: &Path) -> std::io::Result<FileSignature> {
    let meta = fs::metadata(path)?;
    let modified_unix = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(FileSignature {
        size: meta.len(),
        modified_unix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let file_path = dir.path().join("hand.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let sig = signature_of(&file_path).unwrap();

        let mut state = SyncState::load(&state_path);
        assert!(state.needs_sync(&file_path, sig));
        state.mark_synced(file_path.clone(), sig);
        state.save(&state_path).unwrap();

        let reloaded = SyncState::load(&state_path);
        assert!(!reloaded.needs_sync(&file_path, sig));
    }

    #[test]
    fn detects_growth() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hand.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let sig1 = signature_of(&file_path).unwrap();

        let mut state = SyncState::default();
        state.mark_synced(file_path.clone(), sig1);

        std::fs::write(&file_path, "hello world, more hands appended").unwrap();
        let sig2 = signature_of(&file_path).unwrap();
        assert!(state.needs_sync(&file_path, sig2));
    }

    #[test]
    fn loads_state_files_from_before_sent_text_existed() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"synced_files":{"/x/HH.txt":{"size":10,"modified_unix":5}}}"#,
        )
        .unwrap();
        let state = SyncState::load(&state_path);
        let sig = FileSignature { size: 10, modified_unix: 5 };
        assert!(!state.needs_sync(Path::new("/x/HH.txt"), sig));
        assert_eq!(state.sent_text(Path::new("/x/HH.txt")), None);
    }

    #[test]
    fn remembers_how_much_text_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let mut state = SyncState::default();
        let sig = FileSignature { size: 10, modified_unix: 5 };
        let sent = SentText { len: 10, fingerprint: 42 };
        state.mark_synced_text(PathBuf::from("/x/HH.txt"), sig, sent);
        state.save(&state_path).unwrap();

        let reloaded = SyncState::load(&state_path);
        assert_eq!(reloaded.sent_text(Path::new("/x/HH.txt")), Some(sent));
        assert!(!reloaded.needs_sync(Path::new("/x/HH.txt"), sig));
    }

    #[test]
    fn rejected_file_is_set_aside_then_retried() {
        let path = PathBuf::from("/x/HH.txt");
        let sig = FileSignature { size: 10, modified_unix: 1 };
        let mut state = SyncState::default();
        let now = 1_000_000;

        // Erro do servidor: tenta de novo até MAX_FAILED_ATTEMPTS.
        for _ in 1..MAX_FAILED_ATTEMPTS {
            assert!(!state.record_failure(path.clone(), sig, false, now));
            assert!(state.needs_sync_at(&path, sig, now));
        }
        assert!(state.record_failure(path.clone(), sig, false, now));
        assert!(!state.needs_sync_at(&path, sig, now));
        assert_eq!(state.given_up_count(), 1);

        // Volta a tentar depois de um dia, ou se o arquivo mudar.
        assert!(state.needs_sync_at(&path, sig, now + RETRY_GIVEN_UP_AFTER_SECS));
        let grew = FileSignature { size: 20, modified_unix: 2 };
        assert!(state.needs_sync_at(&path, grew, now));

        // Enviado com sucesso: sai da lista de recusados.
        state.mark_synced(path.clone(), grew);
        assert_eq!(state.given_up_count(), 0);
    }

    #[test]
    fn content_rejection_sets_aside_at_once() {
        let path = PathBuf::from("/x/HH.txt");
        let sig = FileSignature { size: 10, modified_unix: 1 };
        let mut state = SyncState::default();
        assert!(state.record_failure(path.clone(), sig, true, 5));
        assert!(!state.needs_sync_at(&path, sig, 5));
    }
}
