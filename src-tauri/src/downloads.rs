//! Importa sozinho o que o jogador baixa do PokerCraft. O GGPoker não grava
//! no computador o resumo dos torneios (prêmio, colocação, bounty): ele só
//! sai no PokerCraft, num .zip. Antes o jogador baixava o .zip e ainda
//! tinha que escolher o arquivo em "Importar arquivos baixados"; agora o
//! Radar olha a pasta Downloads e envia sozinho (a vigilância em tempo real
//! fica em watcher.rs; o envio, em `importar_downloads` no lib.rs).
//!
//! A pasta Downloads tem de tudo, então:
//! - só entram arquivos .zip direto na pasta (sem descer em subpastas);
//! - só os baixados nos últimos 7 dias;
//! - antes de ler o .zip inteiro, o Radar espia só o começo de alguns
//!   arquivos de texto dentro dele (`importar::tem_arquivo_de_poker`): o
//!   .zip que não tem mão nem torneio fica de lado, sem ser lido nem
//!   enviado;
//! - cada .zip é olhado uma vez só (`Vistos`), a não ser que mude.

use scanner::state::{signature_of, FileSignature};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::importar::e_zip;

/// Só o que foi baixado nos últimos 7 dias (pedido explícito): cobre o
/// .zip baixado com o Radar fechado, sem sair abrindo downloads antigos.
pub const JANELA_SEGS: i64 = 7 * 24 * 60 * 60;
/// Arquivo mexido há menos que isso pode ainda estar sendo baixado: fica
/// pra próxima olhada (o navegador avisa de novo quando termina, e o ciclo
/// de 5 minutos é a rede de segurança).
pub const QUIETO_HA_SEGS: i64 = 5;
/// .zip maior que isso não é aberto sozinho (o do PokerCraft é bem menor);
/// continua dando pra escolher em "Importar arquivos baixados".
pub const MAX_ZIP_BYTES: u64 = 200 * 1024 * 1024;

/// Os .zip da pasta Downloads que o Radar já olhou (enviou, ou viu que não
/// eram de poker), com o tamanho e a data de cada um: se o arquivo mudar,
/// é olhado de novo.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vistos {
    #[serde(default)]
    arquivos: HashMap<PathBuf, FileSignature>,
}

impl Vistos {
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
        // Mesmo cuidado do config.json: grava ao lado e troca de uma vez.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)
    }

    pub fn ja_visto(&self, caminho: &Path, sig: FileSignature) -> bool {
        self.arquivos.get(caminho) == Some(&sig)
    }

    pub fn marcar(&mut self, caminho: PathBuf, sig: FileSignature) {
        self.arquivos.insert(caminho, sig);
    }

    /// Esquece o que saiu da janela de 7 dias ou não existe mais, pra a
    /// lista não crescer sem fim.
    pub fn limpar(&mut self, agora: i64) {
        self.arquivos
            .retain(|caminho, sig| sig.modified_unix >= agora - JANELA_SEGS && caminho.is_file());
    }
}

/// Os .zip da pasta que ainda não foram olhados: baixados nos últimos 7
/// dias, parados há alguns segundos (download terminado) e de tamanho
/// razoável. Do mais antigo pro mais novo.
pub fn candidatos(pasta: &Path, vistos: &Vistos, agora: i64) -> Vec<(PathBuf, FileSignature)> {
    let Ok(entradas) = std::fs::read_dir(pasta) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entrada in entradas.flatten() {
        let caminho = entrada.path();
        // `file_type` da entrada não segue atalhos (links): só arquivo comum.
        if !e_zip(&caminho) || !entrada.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(sig) = signature_of(&caminho) else { continue };
        let idade = agora - sig.modified_unix;
        if sig.size == 0 || sig.size > MAX_ZIP_BYTES || !(QUIETO_HA_SEGS..=JANELA_SEGS).contains(&idade) {
            continue;
        }
        if !vistos.ja_visto(&caminho, sig) {
            out.push((caminho, sig));
        }
    }
    out.sort_by_key(|(_, sig)| sig.modified_unix);
    out
}

/// Texto da notificação depois de importar um .zip baixado. Sem nada novo
/// (tudo já estava no PokerSync), não incomoda o jogador. Sem o nome do
/// arquivo: o PokerCraft baixa com um código no lugar do nome
/// ("dacf902c-000001a1-....zip"), que não diz nada pro jogador.
pub fn aviso_de_importacao(maos: u32, torneios: u32) -> Option<String> {
    let partes: Vec<String> = [(torneios, "torneio(s)"), (maos, "mão(s)")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, o_que)| format!("{n} {o_que}"))
        .collect();
    if partes.is_empty() {
        return None;
    }
    Some(format!(
        "Importado da pasta Downloads: {} que ainda não estavam no PokerSync.",
        partes.join(" e ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn agora() -> i64 {
        scanner::state::now_unix()
    }

    fn com_data(caminho: &Path, segundos_atras: u64) {
        let quando = SystemTime::now() - Duration::from_secs(segundos_atras);
        std::fs::File::options()
            .write(true)
            .open(caminho)
            .unwrap()
            .set_modified(quando)
            .unwrap();
    }

    fn arquivo(dir: &Path, nome: &str, segundos_atras: u64) -> PathBuf {
        let caminho = dir.join(nome);
        std::fs::write(&caminho, "PK conteúdo").unwrap();
        com_data(&caminho, segundos_atras);
        caminho
    }

    #[test]
    fn picks_recent_finished_zips_only() {
        let dir = tempfile::tempdir().unwrap();
        let ontem = arquivo(dir.path(), "PokerCraft.zip", 24 * 60 * 60);
        let hoje = arquivo(dir.path(), "outro.ZIP", 60);
        arquivo(dir.path(), "antigo.zip", 8 * 24 * 60 * 60);
        arquivo(dir.path(), "baixando.zip", 0);
        arquivo(dir.path(), "PokerCraft.zip.crdownload", 60);
        arquivo(dir.path(), "notas.txt", 60);
        std::fs::create_dir(dir.path().join("pasta.zip")).unwrap();
        std::fs::write(dir.path().join("vazio.zip"), "").unwrap();
        com_data(&dir.path().join("vazio.zip"), 60);

        let achados: Vec<PathBuf> = candidatos(dir.path(), &Vistos::default(), agora())
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        // Do mais antigo pro mais novo.
        assert_eq!(achados, vec![ontem, hoje]);
    }

    #[test]
    fn seen_zips_come_back_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let zip = arquivo(dir.path(), "PokerCraft.zip", 60);
        let mut vistos = Vistos::default();
        let (caminho, sig) = candidatos(dir.path(), &vistos, agora()).remove(0);
        vistos.marcar(caminho, sig);
        assert!(candidatos(dir.path(), &vistos, agora()).is_empty());

        // Baixou de novo por cima (outro tamanho): olha de novo.
        std::fs::write(&zip, "PK conteúdo novo e maior").unwrap();
        com_data(&zip, 30);
        assert_eq!(candidatos(dir.path(), &vistos, agora()).len(), 1);

        // Gravado e lido de volta do disco.
        let memoria = dir.path().join("estado").join("downloads-vistos.json");
        vistos.save(&memoria).unwrap();
        assert!(Vistos::load(&memoria).ja_visto(&dir.path().join("PokerCraft.zip"), sig));
    }

    #[test]
    fn forgets_old_and_deleted_zips() {
        let dir = tempfile::tempdir().unwrap();
        let fica = arquivo(dir.path(), "fica.zip", 60);
        let apagado = arquivo(dir.path(), "apagado.zip", 60);
        let velho = arquivo(dir.path(), "velho.zip", 8 * 24 * 60 * 60);
        let mut vistos = Vistos::default();
        for c in [&fica, &apagado, &velho] {
            vistos.marcar(c.clone(), signature_of(c).unwrap());
        }
        std::fs::remove_file(&apagado).unwrap();
        vistos.limpar(agora());
        assert_eq!(vistos.arquivos.keys().collect::<Vec<_>>(), vec![&fica]);
    }

    #[test]
    fn notification_text_only_with_news() {
        assert_eq!(aviso_de_importacao(0, 0), None);
        assert_eq!(
            aviso_de_importacao(340, 12).unwrap(),
            "Importado da pasta Downloads: 12 torneio(s) e 340 mão(s) que ainda não estavam no PokerSync."
        );
        assert!(aviso_de_importacao(0, 3).unwrap().contains(": 3 torneio(s) que"));
    }
}
