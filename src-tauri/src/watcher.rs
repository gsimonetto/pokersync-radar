//! Vigilância das pastas de hand history em tempo real — o mesmo
//! "auto-import" do PokerTracker/Holdem Manager. Quando o cliente de poker
//! grava uma mão, o sistema operacional avisa e o Radar sincroniza logo em
//! seguida, em vez de esperar o ciclo de 5 minutos (que continua rodando
//! como rede de segurança: pasta criada depois, aviso perdido etc.).
//! A pasta Downloads tem a sua própria vigia, que só liga pra .zip (o
//! que o jogador baixa do PokerCraft — ver downloads.rs).

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

/// Arquivo criado ou alterado (troca de nome conta: o navegador baixa com
/// outro nome e renomeia no fim). Leitura e mudança só de atributos ficam
/// de fora — senão o próprio Radar, ao ler os arquivos pra enviar, se
/// acordaria de novo sem fim.
fn escreveu(event: &Event) -> bool {
    match event.kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        _ => false,
    }
}

/// Aviso que interessa: arquivo de hand history criado ou alterado.
pub fn mudou_arquivo_de_mao(event: &Event) -> bool {
    escreveu(event) && event.paths.iter().any(|p| scanner::has_text_extension(p))
}

/// Aviso que interessa na pasta Downloads: um .zip que chegou ou mudou.
pub fn mudou_zip_baixado(event: &Event) -> bool {
    escreveu(event) && event.paths.iter().any(|p| crate::importar::e_zip(p))
}

pub struct Vigia {
    /// Que avisos do sistema contam (ver `mudou_arquivo_de_mao`).
    filtro: fn(&Event) -> bool,
    modo: RecursiveMode,
    watcher: Option<RecommendedWatcher>,
    /// As pastas pedidas na última vez (pra não recriar sem necessidade).
    pedidas: Vec<PathBuf>,
    vigiadas: usize,
}

impl Vigia {
    fn nova(filtro: fn(&Event) -> bool, modo: RecursiveMode) -> Self {
        Vigia {
            filtro,
            modo,
            watcher: None,
            pedidas: Vec::new(),
            vigiadas: 0,
        }
    }

    /// Pastas de hand history, com as subpastas.
    pub fn de_maos() -> Self {
        Self::nova(mudou_arquivo_de_mao, RecursiveMode::Recursive)
    }

    /// Pasta Downloads: só os .zip direto nela (sem descer em subpastas).
    pub fn de_downloads() -> Self {
        Self::nova(mudou_zip_baixado, RecursiveMode::NonRecursive)
    }

    /// Passa a vigiar `pastas` (só refaz se a lista mudou). Cada mudança
    /// que passa no filtro manda um aviso em `aviso`. Devolve quantas
    /// pastas estão sendo vigiadas.
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
        let filtro = self.filtro;
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if filtro(&event) {
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
            match watcher.watch(pasta, self.modo) {
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
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RenameMode};

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
    fn only_zips_count_in_downloads() {
        // O navegador termina o download trocando o nome pra .zip.
        let renomeou = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/dl/PokerCraft.zip.crdownload"))
            .add_path(PathBuf::from("/dl/PokerCraft.zip"));
        assert!(mudou_zip_baixado(&renomeou));
        assert!(mudou_zip_baixado(&evento(EventKind::Create(CreateKind::File), "/dl/a.ZIP")));
        assert!(!mudou_zip_baixado(&evento(EventKind::Create(CreateKind::File), "/dl/a.zip.part")));
        assert!(!mudou_zip_baixado(&evento(EventKind::Create(CreateKind::File), "/dl/HH1.txt")));
        assert!(!mudou_zip_baixado(&evento(EventKind::Access(AccessKind::Any), "/dl/a.zip")));
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
        let mut vigia = Vigia::de_maos();
        assert_eq!(vigia.atualizar(vec![dir.path().to_path_buf()], &tx), 1);

        std::fs::write(dir.path().join("notas.ini"), "x").unwrap();
        std::fs::write(dir.path().join("HH novo.txt"), "PokerStars Hand #1:\n").unwrap();
        let chegou = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(chegou, Ok(Some(()))), "nenhum aviso da vigilância");

        // Mesma lista: não recria (continua vigiando).
        assert_eq!(vigia.atualizar(vec![dir.path().to_path_buf()], &tx), 1);
        assert_eq!(vigia.atualizar(Vec::new(), &tx), 0);
    }

    #[tokio::test]
    async fn warns_when_a_zip_is_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut vigia = Vigia::de_downloads();
        assert_eq!(vigia.atualizar(vec![dir.path().to_path_buf()], &tx), 1);

        // Fora da conta: outro tipo de arquivo e .zip em subpasta.
        std::fs::write(dir.path().join("foto.jpg"), "x").unwrap();
        std::fs::write(sub.join("dentro.zip"), "PK").unwrap();
        let nada = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(nada.is_err(), "aviso que não devia");

        // Download terminando: arquivo temporário renomeado pra .zip.
        let temp = dir.path().join("PokerCraft.zip.crdownload");
        std::fs::write(&temp, "PK").unwrap();
        std::fs::rename(&temp, dir.path().join("PokerCraft.zip")).unwrap();
        let chegou = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(chegou, Ok(Some(()))), "nenhum aviso do .zip baixado");
    }
}
