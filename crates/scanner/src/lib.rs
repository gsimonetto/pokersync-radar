//! Varredura de hand history no disco do usuário. Não faz parsing de mão —
//! isso já existe (e é validado contra formatos reais) no backend, em
//! lib/poker/hand-parser.ts. Este crate só encontra arquivos plausíveis,
//! evita reenviar o que não mudou, e devolve texto bruto pra sincronizar.

pub mod cache;
pub mod room;
pub mod state;
pub mod text;

pub use cache::ClassCache;
pub use room::{FileKind, PokerRoom};
pub use state::{signature_of, FileSignature, SentText, SyncState};

use std::collections::HashSet;
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

/// Lê só o começo do arquivo (o bastante pra reconhecer a sala).
fn read_head(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; SNIFF_BYTES];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(text::decode_prefix(&buf))
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

/// Pasta pra varrer. `hint` = sala dona da pasta (pasta padrão dela);
/// `None` pras pastas que o jogador adicionou, onde só o conteúdo decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRoot {
    pub path: PathBuf,
    pub hint: Option<PokerRoom>,
}

/// Todas as pastas a varrer pra esse tipo de arquivo: as padrão de cada
/// sala (com a sala como dica) e depois as que o jogador adicionou. Pasta
/// repetida entra uma vez só.
pub fn search_roots(kind: FileKind, extra: &[PathBuf]) -> Vec<SearchRoot> {
    let mut seen = HashSet::new();
    PokerRoom::ALL
        .into_iter()
        .flat_map(|room| {
            room.default_search_paths(kind)
                .into_iter()
                .map(move |path| SearchRoot { path, hint: Some(room) })
        })
        .chain(extra.iter().cloned().map(|path| SearchRoot { path, hint: None }))
        .filter(|r| seen.insert(r.path.clone()))
        .collect()
}

/// Varre `roots` recursivamente procurando hand history (ou resumo de
/// torneio, conforme `kind`) de todas as salas de uma vez. Cada pasta é
/// percorrida uma vez só e cada arquivo fica com uma sala só (ver
/// `PokerRoom::classify`) — antes cada pasta era percorrida uma vez por
/// sala. Só lê os primeiros bytes de arquivos novos ou que mudaram; os já
/// reconhecidos vêm de `cache`. O conteúdo inteiro só é lido na hora do
/// envio, e só pros arquivos que realmente precisam sincronizar.
pub fn discover(roots: &[SearchRoot], kind: FileKind, cache: &mut ClassCache) -> Vec<DiscoveredFile> {
    let mut found = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        if !root.path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&root.path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if !has_text_extension(path) || visited.contains(path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(signature) = signature_of(path) else {
                continue;
            };
            visited.insert(path.to_path_buf());
            let room = match cache.get(path, signature) {
                Some(room) => room,
                None => {
                    let Some(head) = read_head(path) else { continue };
                    let room = PokerRoom::classify(kind, &head, root.hint);
                    cache.put(path, signature, room);
                    room
                }
            };
            if let Some(room) = room {
                found.push(DiscoveredFile {
                    path: path.to_path_buf(),
                    room,
                    signature,
                });
            }
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

    fn discover_files(roots: &[PathBuf], room: PokerRoom, kind: FileKind) -> Vec<DiscoveredFile> {
        let roots: Vec<SearchRoot> = roots
            .iter()
            .map(|p| SearchRoot {
                path: p.clone(),
                hint: Some(room),
            })
            .collect();
        discover(&roots, kind, &mut ClassCache::new())
    }

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

    fn extra_root(path: &Path) -> Vec<SearchRoot> {
        vec![SearchRoot {
            path: path.to_path_buf(),
            hint: None,
        }]
    }

    #[test]
    fn same_folder_in_both_kinds_splits_hands_and_summaries() {
        // O caso do ACR: mãos e resumos na mesma pasta, adicionada em
        // "Importar mãos" e em "Importar torneios".
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "HH20260921 T24305619.txt",
            "Hand #2134567891 - Tournament #24305619 - Holdem(No Limit) - Level 1 (10.00/20.00) - 2026/09/21 18:00:05 UTC\n",
        );
        write(
            dir.path(),
            "HH PokerStars.txt",
            "PokerStars Hand #1234: Tournament #555, $10+$1 USD Hold'em No Limit\n",
        );
        write(
            dir.path(),
            "TS PokerStars.txt",
            "PokerStars Tournament #555, No Limit Hold'em\nBuy-In: $10.00+$1.00\n",
        );

        let hands = discover(&extra_root(dir.path()), FileKind::HandHistory, &mut ClassCache::new());
        let mut rooms: Vec<_> = hands.iter().map(|f| f.room).collect();
        rooms.sort_by_key(|r| r.slug());
        assert_eq!(rooms, vec![PokerRoom::Acr, PokerRoom::PokerStars]);

        let summaries = discover(&extra_root(dir.path()), FileKind::TournamentSummary, &mut ClassCache::new());
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].path.ends_with("TS PokerStars.txt"));
        assert_eq!(summaries[0].room, PokerRoom::PokerStars);
    }

    #[test]
    fn nested_roots_do_not_duplicate_files() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("PokerStars");
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub, "HH.txt", "PokerStars Hand #1: Hold'em No Limit\n");
        let roots = vec![
            SearchRoot {
                path: sub.clone(),
                hint: Some(PokerRoom::PokerStars),
            },
            SearchRoot {
                path: dir.path().to_path_buf(),
                hint: None,
            },
        ];
        assert_eq!(discover(&roots, FileKind::HandHistory, &mut ClassCache::new()).len(), 1);
    }

    #[test]
    fn cache_skips_reopening_unchanged_files_and_forgets_deleted_ones() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache").join("classificacao.json");
        let hands = dir.path().join("hands");
        std::fs::create_dir_all(&hands).unwrap();
        let hh = write(&hands, "HH.txt", "PokerStars Hand #1: Hold'em No Limit\n");
        write(&hands, "notes.txt", "não é mão");

        let mut cache = ClassCache::load(&cache_path);
        assert_eq!(discover(&extra_root(&hands), FileKind::HandHistory, &mut cache).len(), 1);
        cache.save_if_changed(&cache_path).unwrap();

        // Troca o conteúdo por algo irreconhecível, mantendo tamanho e
        // data: se o arquivo continua achado, veio da memória (não foi
        // aberto de novo).
        let original = std::fs::read(&hh).unwrap();
        let mtime = std::fs::metadata(&hh).unwrap().modified().unwrap();
        std::fs::write(&hh, vec![b'x'; original.len()]).unwrap();
        std::fs::File::options().write(true).open(&hh).unwrap().set_modified(mtime).unwrap();
        let mut cache = ClassCache::load(&cache_path);
        assert_eq!(discover(&extra_root(&hands), FileKind::HandHistory, &mut cache).len(), 1);

        // Mudou de verdade (data nova): é reconhecido de novo.
        std::fs::File::options()
            .write(true)
            .open(&hh)
            .unwrap()
            .set_modified(mtime + std::time::Duration::from_secs(10))
            .unwrap();
        let mut cache = ClassCache::load(&cache_path);
        assert!(discover(&extra_root(&hands), FileKind::HandHistory, &mut cache).is_empty());

        std::fs::remove_file(&hh).unwrap();
        let mut cache = ClassCache::load(&cache_path);
        assert!(discover(&extra_root(&hands), FileKind::HandHistory, &mut cache).is_empty());
        cache.save_if_changed(&cache_path).unwrap();
        let saved = std::fs::read_to_string(&cache_path).unwrap();
        assert!(!saved.contains("HH.txt"));
        assert!(saved.contains("notes.txt"));
    }
}
