//! Varredura de hand history no disco do usuário. Não faz parsing de mão —
//! isso já existe (e é validado contra formatos reais) no backend, em
//! lib/poker/hand-parser.ts. Este crate só encontra arquivos plausíveis,
//! evita reenviar o que não mudou, e devolve texto bruto pra sincronizar.

pub mod room;
pub mod state;
pub mod text;

pub use room::{FileKind, PokerRoom};
pub use state::{signature_of, FileSignature, SentText, SyncState};

use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024; // hand history de anos ainda cabe tranquilo
const SNIFF_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub room: PokerRoom,
    pub signature: FileSignature,
}

#[derive(Debug, Clone)]
pub struct PendingFile {
    pub path: PathBuf,
    pub room: PokerRoom,
    pub content: String,
    pub signature: FileSignature,
}

fn has_text_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("txt") || e.eq_ignore_ascii_case("log"))
        .unwrap_or(false)
}

fn sniff_file(path: &Path, room: PokerRoom, kind: FileKind) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; SNIFF_BYTES];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    buf.truncate(n);
    room.sniff_kind(kind, &text::decode_prefix(&buf))
}

/// Lê o arquivo inteiro em qualquer codificação comum (ver
/// `text::decode_text`) — antes, arquivo que não fosse UTF-8 era pulado
/// sem aviso.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    Ok(text::decode_text(&std::fs::read(path)?))
}

/// Só os arquivos que mudaram desde o último envio, sem ler o conteúdo —
/// quem chama lê um por vez (`read_text`), pra não carregar um histórico
/// inteiro na memória de uma vez.
pub fn pending_files<'a>(files: &'a [DiscoveredFile], state: &SyncState) -> Vec<&'a DiscoveredFile> {
    files
        .iter()
        .filter(|f| state.needs_sync(&f.path, f.signature))
        .collect()
}

/// Varre `roots` recursivamente procurando hand history (ou resumo de
/// torneio, conforme `kind`) da sala indicada. Só lê os primeiros bytes de
/// cada arquivo candidato (sniff) — o conteúdo inteiro só é lido depois,
/// em `read_pending`, e só pros arquivos que realmente precisam
/// sincronizar.
pub fn discover_files(roots: &[PathBuf], room: PokerRoom, kind: FileKind) -> Vec<DiscoveredFile> {
    let mut found = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if !has_text_extension(path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            if !sniff_file(path, room, kind) {
                continue;
            }
            let Ok(signature) = signature_of(path) else {
                continue;
            };
            found.push(DiscoveredFile {
                path: path.to_path_buf(),
                room,
                signature,
            });
        }
    }
    found
}

/// Filtra `files` pelos que mudaram desde o último sync (via `state`) e lê
/// o conteúdo inteiro só desses.
pub fn read_pending(files: &[DiscoveredFile], state: &SyncState) -> Vec<PendingFile> {
    pending_files(files, state)
        .into_iter()
        .filter_map(|f| {
            read_text(&f.path)
                .ok()
                .map(|content| PendingFile {
                    path: f.path.clone(),
                    room: f.room,
                    content,
                    signature: f.signature,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn discovers_pokerstars_hand_history_and_ignores_junk() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "HH20260101 Table.txt",
            "PokerStars Hand #1234: Tournament #1, $10+$1 USD Hold'em No Limit\n...",
        );
        write(
            dir.path(),
            "readme.md",
            "PokerStars Hand #1234: not a hand history file, wrong extension",
        );
        write(
            dir.path(),
            "notes.txt",
            "just some notes, not a hand history",
        );
        write(dir.path(), "empty.txt", "");

        let found = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("HH20260101 Table.txt"));
    }

    #[test]
    fn recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("2026-08");
        std::fs::create_dir_all(&sub).unwrap();
        write(
            &sub,
            "HH.txt",
            "PokerStars Hand #999: Hold'em No Limit\n...",
        );

        let found = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn missing_root_is_skipped_not_error() {
        let found = discover_files(
            &[PathBuf::from("/this/path/does/not/exist")],
            PokerRoom::PokerStars,
            FileKind::HandHistory,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn read_pending_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "HH.txt",
            "PokerStars Hand #1: Hold'em No Limit\n...",
        );
        let found = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        assert_eq!(found.len(), 1);

        let mut state = SyncState::default();
        let pending_before = read_pending(&found, &state);
        assert_eq!(pending_before.len(), 1);

        state.mark_synced(path.clone(), found[0].signature);
        let pending_after = read_pending(&found, &state);
        assert!(pending_after.is_empty());
    }

    #[test]
    fn discovers_tournament_summary_separately_from_hand_history() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "TS1234567890.txt",
            "PokerStars Tournament #1234567890, No Limit Hold'em\nBuy-In: $10.00+$1.00\n...",
        );
        write(
            dir.path(),
            "HH1234567890.txt",
            "PokerStars Hand #1234: Tournament #1234567890, $10+$1 USD Hold'em No Limit\n...",
        );

        let hands = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        assert_eq!(hands.len(), 1);
        assert!(hands[0].path.ends_with("HH1234567890.txt"));

        let tournaments = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::TournamentSummary);
        assert_eq!(tournaments.len(), 1);
        assert!(tournaments[0].path.ends_with("TS1234567890.txt"));
    }

    #[test]
    fn finds_and_reads_utf16_hand_history() {
        // Antes: arquivo em UTF-16 nem era reconhecido (o "cheiro" do
        // começo vinha embaralhado) e, se fosse, era pulado na leitura.
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFF, 0xFE];
        for u in "PokerStars Hand #77: Hold'em No Limit\nSeat 1: João\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let path = dir.path().join("HH utf16.txt");
        std::fs::write(&path, bytes).unwrap();

        let found = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        assert_eq!(found.len(), 1);
        let pending = read_pending(&found, &SyncState::default());
        assert_eq!(pending.len(), 1);
        assert!(pending[0].content.contains("Seat 1: João"));
    }

    #[test]
    fn reads_latin1_file_instead_of_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"PokerStars Hand #78: Hold'em No Limit\nSeat 1: Jo".to_vec();
        bytes.push(0xE3); // "ã" em Latin-1
        bytes.extend_from_slice(b"o\n");
        std::fs::write(dir.path().join("HH latin1.txt"), bytes).unwrap();

        let found = discover_files(&[dir.path().to_path_buf()], PokerRoom::PokerStars, FileKind::HandHistory);
        let pending = read_pending(&found, &SyncState::default());
        assert_eq!(pending.len(), 1);
        assert!(pending[0].content.contains("Seat 1: João"));
    }
}
