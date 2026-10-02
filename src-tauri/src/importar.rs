//! "Importar arquivos baixados" — o "importar do disco" do PokerTracker/
//! Holdem Manager. Serve principalmente pro GGPoker, que deixa mãos e
//! resultados de torneio no PokerCraft em vez de gravar no computador: o
//! jogador baixa o .zip lá e escolhe aqui. Também aceita .txt soltos de
//! qualquer sala. A sala e o tipo de cada arquivo saem do conteúdo, igual
//! à varredura das pastas (`PokerRoom::classify`).

use scanner::text::{decode_text, fingerprint};
use scanner::{has_text_extension, FileKind, PokerRoom};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Mesmo teto por arquivo da varredura das pastas.
const MAX_ARQUIVO_BYTES: u64 = 20 * 1024 * 1024;
/// Teto do que é lido numa importação (somando tudo, já descompactado) —
/// protege contra um .zip que "explode" ao abrir.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Quanto do começo do texto olhar pra reconhecer a sala.
const CABECALHO_CHARS: usize = 4096;

/// Arquivos de uma mesma sala e tipo, prontos pra enviar.
#[derive(Debug)]
pub struct Grupo {
    pub kind: FileKind,
    pub room: PokerRoom,
    pub textos: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Leitura {
    pub grupos: Vec<Grupo>,
    /// Arquivos de texto lidos (dentro dos .zip também).
    pub arquivos_lidos: u32,
    /// Lidos mas que nenhuma sala reconhece (ou repetidos/grandes demais).
    pub ignorados: u32,
}

impl Leitura {
    fn adicionar(&mut self, kind: FileKind, room: PokerRoom, texto: String) {
        match self.grupos.iter_mut().find(|g| g.kind == kind && g.room == room) {
            Some(g) => g.textos.push(texto),
            None => self.grupos.push(Grupo {
                kind,
                room,
                textos: vec![texto],
            }),
        }
    }
}

fn cabecalho(texto: &str) -> &str {
    match texto.char_indices().nth(CABECALHO_CHARS) {
        Some((i, _)) => &texto[..i],
        None => texto,
    }
}

/// Sala e tipo de um texto: mãos primeiro, depois resumo de torneio.
pub fn reconhecer(texto: &str) -> Option<(FileKind, PokerRoom)> {
    let head = cabecalho(texto);
    [FileKind::HandHistory, FileKind::TournamentSummary]
        .into_iter()
        .find_map(|kind| PokerRoom::classify(kind, head, None).map(|room| (kind, room)))
}

fn e_zip(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Lê os arquivos escolhidos (.zip e .txt/.log), reconhece cada um e
/// agrupa por sala e tipo. Texto repetido (o mesmo arquivo escolhido duas
/// vezes, ou solto e dentro do .zip) entra uma vez só.
pub fn ler_escolhidos(caminhos: &[PathBuf]) -> Result<Leitura, String> {
    let mut leitura = Leitura::default();
    let mut total: u64 = 0;
    let mut vistos: HashSet<u64> = HashSet::new();
    let mut aceitar = |leitura: &mut Leitura, bytes: Vec<u8>| {
        leitura.arquivos_lidos += 1;
        let texto = decode_text(&bytes);
        if texto.trim().is_empty() || !vistos.insert(fingerprint(&texto)) {
            leitura.ignorados += 1;
            return;
        }
        match reconhecer(&texto) {
            Some((kind, room)) => leitura.adicionar(kind, room, texto),
            None => leitura.ignorados += 1,
        }
    };

    for caminho in caminhos {
        if e_zip(caminho) {
            let arquivo = std::fs::File::open(caminho)
                .map_err(|e| format!("Não consegui abrir {}: {e}", nome(caminho)))?;
            let mut zip = zip::ZipArchive::new(arquivo)
                .map_err(|_| format!("{} não é um .zip válido.", nome(caminho)))?;
            for i in 0..zip.len() {
                let Ok(mut entrada) = zip.by_index(i) else { continue };
                if !entrada.is_file() || !has_text_extension(Path::new(entrada.name())) {
                    continue;
                }
                if entrada.size() > MAX_ARQUIVO_BYTES {
                    leitura.arquivos_lidos += 1;
                    leitura.ignorados += 1;
                    continue;
                }
                // `take`: não confia no tamanho declarado dentro do .zip.
                let mut bytes = Vec::new();
                if (&mut entrada).take(MAX_ARQUIVO_BYTES + 1).read_to_end(&mut bytes).is_err()
                    || bytes.len() as u64 > MAX_ARQUIVO_BYTES
                {
                    leitura.arquivos_lidos += 1;
                    leitura.ignorados += 1;
                    continue;
                }
                total += bytes.len() as u64;
                if total > MAX_TOTAL_BYTES {
                    return Err(grande_demais());
                }
                aceitar(&mut leitura, bytes);
            }
        } else if has_text_extension(caminho) {
            let tamanho = std::fs::metadata(caminho).map(|m| m.len()).unwrap_or(0);
            if tamanho > MAX_ARQUIVO_BYTES {
                leitura.arquivos_lidos += 1;
                leitura.ignorados += 1;
                continue;
            }
            total += tamanho;
            if total > MAX_TOTAL_BYTES {
                return Err(grande_demais());
            }
            let bytes = std::fs::read(caminho).map_err(|e| format!("Não consegui ler {}: {e}", nome(caminho)))?;
            aceitar(&mut leitura, bytes);
        }
    }
    Ok(leitura)
}

fn nome(caminho: &Path) -> String {
    caminho
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| caminho.display().to_string())
}

fn grande_demais() -> String {
    "Arquivos grandes demais pra uma importação só — escolha menos arquivos por vez.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const GG_MAO: &str = "Poker Hand #TM123: Tournament #9001, Bounty Hunters $10 Hold'em No Limit - Level1(10/20) - 2026/09/21 18:00:05\nTable '9' 8-max Seat #1 is the button\n";
    const GG_RESUMO: &str = "Tournament #9001, Bounty Hunters $10, Hold'em No Limit\nBuy-in: $9.6+$0.4\n150 Players\nYou finished the tournament in 12th place.\n";
    const PS_MAO: &str = "PokerStars Hand #1: Hold'em No Limit ($0.01/$0.02) - 2026/09/21\n";

    fn zip_com(dir: &Path, nome: &str, arquivos: &[(&str, &str)]) -> PathBuf {
        let caminho = dir.join(nome);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&caminho).unwrap());
        let opcoes = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (nome, conteudo) in arquivos {
            zip.start_file(*nome, opcoes).unwrap();
            zip.write_all(conteudo.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        caminho
    }

    #[test]
    fn reads_pokercraft_zip_and_loose_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip = zip_com(
            dir.path(),
            "PokerCraft.zip",
            &[
                ("hands/GG20260921 - Tournament #9001.txt", GG_MAO),
                ("summaries/GG20260921 - Tournament #9001.txt", GG_RESUMO),
                ("leia-me.pdf", "não é texto de mão"),
                ("notas.txt", "nada a ver"),
            ],
        );
        let solto = dir.path().join("HH PokerStars.txt");
        std::fs::write(&solto, PS_MAO).unwrap();

        let leitura = ler_escolhidos(&[zip, solto.clone(), solto]).unwrap();
        // 3 .txt do zip + o solto 2x (o .pdf nem conta).
        assert_eq!(leitura.arquivos_lidos, 5);
        assert_eq!(leitura.ignorados, 2); // notas.txt + o solto repetido
        let mut grupos: Vec<_> = leitura
            .grupos
            .iter()
            .map(|g| (g.room.slug(), g.kind.slug(), g.textos.len()))
            .collect();
        grupos.sort();
        assert_eq!(
            grupos,
            vec![("ggpoker", "hands", 1), ("ggpoker", "tournaments", 1), ("pokerstars", "hands", 1)]
        );
    }

    #[test]
    fn invalid_zip_is_a_friendly_error() {
        let dir = tempfile::tempdir().unwrap();
        let falso = dir.path().join("baixado.zip");
        std::fs::write(&falso, "isto não é zip").unwrap();
        let erro = ler_escolhidos(&[falso]).unwrap_err();
        assert!(erro.contains("não é um .zip válido"), "{erro}");
    }
}
