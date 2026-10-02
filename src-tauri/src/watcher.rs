//! Vigilância das pastas de hand history em tempo real — o mesmo
//! "auto-import" do PokerTracker/Holdem Manager. Quando o cliente de poker
//! grava uma mão, o sistema operacional avisa e o Radar sincroniza logo em
//! seguida, em vez de esperar o ciclo de 5 minutos (que continua rodando
//! como rede de segurança: pasta criada depois, aviso perdido etc.).

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Espera depois da primeira mudança antes de sincronizar: junta num envio
/// só as mãos que o cliente grava em sequência (várias mesas abertas) e dá
/// tempo de a mão terminar de ser escrita. Também limita a um ciclo a cada
/// ~20 s jogando, bem abaixo do limite de envios por minuto do site.
pub const ESPERA_APOS_MUDANCA: Duration = Duration::from_secs(20);

/// Pastas que existem no disco, sem repetir as que já estão dentro de
/// outra da lista (a vigilância é recursiva — vigiar as duas dobraria os
/// avisos).
pub fn pastas_para_vigiar(candidatas: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut existentes: Vec<PathBuf> = candidatas.into_iter().filter(|p| p.is_dir()).collect();
    existentes.sort();
    existentes.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in existentes {
        // Ordenado: uma pasta "mãe" sempre vem antes das que estão dentro dela.
        if !out.iter().any(|mae| p.starts_with(mae)) {
            out.push(p);
        }
    }
    out
}

/// Aviso que interessa: arquivo de hand history criado ou alterado. Leitura
/// e mudança só de atributos ficam de fora — senão o próprio Radar, ao ler
/// os arquivos pra enviar, se acordaria de novo sem fim.
pub fn mudou_arquivo_de_mao(event: &Event) -> bool {
    let tipo_certo = match event.kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    };
    tipo_certo && event.paths.iter().any(|p| scanner::has_text_extension(p))
}

#[derive(Default)]
pub struct Vigia {
    watcher: Option<RecommendedWatcher>,
    /// As pastas pedidas na última vez (pra não recriar sem necessidade).
    pedidas: Vec<PathBuf>,
    vigiadas: usize,
}

impl Vigia {
    /// Passa a vigiar `pastas` (só refaz se a lista mudou). Cada mudança
    /// de arquivo de mão manda um aviso em `aviso`. Devolve quantas pastas
    /// estão sendo vigiadas.
    pub fn atualizar(&mut self, pastas: Vec<PathBuf>, aviso: &UnboundedSender<()>) -> usize {
        if pastas == self.pedidas {
            return self.vigiadas;
        }
        self.watcher = None;
        self.vigiadas = 0;
        self.pedidas = pastas.clone();
        if pastas.is_empty() {
            return 0;
        }
        let aviso = aviso.clone();
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if mudou_arquivo_de_mao(&event) {
                    let _ = aviso.send(());
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[radar] não consegui vigiar as pastas (fica só o ciclo de 5 min): {e}");
                return 0;
            }
        };
        for pasta in &pastas {
            match watcher.watch(pasta, RecursiveMode::Recursive) {
                Ok(()) => self.vigiadas += 1,
                Err(e) => eprintln!("[radar] não consegui vigiar {}: {e}", pasta.display()),
            }
        }
        self.watcher = Some(watcher);
        self.vigiadas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind};

    fn evento(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn only_hand_files_written_count() {
        assert!(mudou_arquivo_de_mao(&evento(EventKind::Create(CreateKind::File), "/hh/HH1.txt")));
        assert!(mudou_arquivo_de_mao(&evento(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            "/hh/HH1.txt"
        )));
        assert!(mudou_arquivo_de_mao(&evento(EventKind::Modify(ModifyKind::Any), "/hh/HH1.TXT")));
        // Leitura do próprio Radar e atributos: não acordam o envio.
        assert!(!mudou_arquivo_de_mao(&evento(EventKind::Access(AccessKind::Any), "/hh/HH1.txt")));
        assert!(!mudou_arquivo_de_mao(&evento(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            "/hh/HH1.txt"
        )));
        // Outros arquivos da pasta (config do cliente, imagens…).
        assert!(!mudou_arquivo_de_mao(&evento(EventKind::Create(CreateKind::File), "/hh/user.ini")));
    }

    #[test]
    fn watches_existing_folders_once() {
        let dir = tempfile::tempdir().unwrap();
        let mae = dir.path().join("PokerStars");
        let filha = mae.join("HandHistory");
        let outra = dir.path().join("GGPoker");
        std::fs::create_dir_all(&filha).unwrap();
        std::fs::create_dir_all(&outra).unwrap();
        let pastas = pastas_para_vigiar([
            filha.clone(),
            dir.path().join("nao-existe"),
            mae.clone(),
            outra.clone(),
            outra.clone(),
        ]);
        assert_eq!(pastas, vec![outra, mae]);
    }

    #[tokio::test]
    async fn warns_when_a_hand_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut vigia = Vigia::default();
        assert_eq!(vigia.atualizar(vec![dir.path().to_path_buf()], &tx), 1);

        std::fs::write(dir.path().join("notas.ini"), "x").unwrap();
        std::fs::write(dir.path().join("HH novo.txt"), "PokerStars Hand #1:\n").unwrap();
        let chegou = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(chegou, Ok(Some(()))), "nenhum aviso da vigilância");

        // Mesma lista: não recria (continua vigiando).
        assert_eq!(vigia.atualizar(vec![dir.path().to_path_buf()], &tx), 1);
        assert_eq!(vigia.atualizar(Vec::new(), &tx), 0);
    }
}
