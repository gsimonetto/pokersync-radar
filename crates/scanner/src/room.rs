use std::path::PathBuf;

use crate::text::looks_like_hand_history;

/// Dois tipos de arquivo que o agente varre — hand history (mãos jogadas)
/// e resumo de torneio (buy-in/colocação/premiação, sem as mãos). São
/// arquivos diferentes, em pastas diferentes, e alimentam endpoints
/// diferentes no backend (ver `kind_to_endpoint` no crate `sync-client` e
/// os dois botões "Importar mãos"/"Importar torneios" na UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    HandHistory,
    TournamentSummary,
}

impl FileKind {
    pub fn slug(self) -> &'static str {
        match self {
            FileKind::HandHistory => "hands",
            FileKind::TournamentSummary => "tournaments",
        }
    }

    pub fn from_slug(slug: &str) -> Option<FileKind> {
        match slug {
            "hands" => Some(FileKind::HandHistory),
            "tournaments" => Some(FileKind::TournamentSummary),
            _ => None,
        }
    }
}

/// Salas de poker suportadas no MVP do agente. `slug()` é o valor enviado
/// pro backend (coluna `poker_room` de `hand_reviews`/`hand_sync_batches`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PokerRoom {
    PokerStars,
    GgPoker,
    PartyPoker,
    Poker888,
    Acr,
}

impl PokerRoom {
    pub const ALL: [PokerRoom; 5] = [
        PokerRoom::PokerStars,
        PokerRoom::GgPoker,
        PokerRoom::PartyPoker,
        PokerRoom::Poker888,
        PokerRoom::Acr,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            PokerRoom::PokerStars => "pokerstars",
            PokerRoom::GgPoker => "ggpoker",
            PokerRoom::PartyPoker => "partypoker",
            PokerRoom::Poker888 => "888poker",
            PokerRoom::Acr => "acr",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            PokerRoom::PokerStars => "PokerStars",
            PokerRoom::GgPoker => "GGPoker",
            PokerRoom::PartyPoker => "PartyPoker",
            PokerRoom::Poker888 => "888poker",
            PokerRoom::Acr => "ACR",
        }
    }

    pub fn from_slug(slug: &str) -> Option<PokerRoom> {
        PokerRoom::ALL.into_iter().find(|r| r.slug() == slug)
    }

    /// Nomes de pasta conhecidos do cliente dessa sala, por variação de
    /// skin/país — hand history sempre fica numa subpasta "HandHistory"
    /// dentro dela. Best-effort: cada operadora muda isso sem aviso, então
    /// isto é só o ponto de partida da varredura — o usuário pode (e deve
    /// poder) apontar pastas adicionais na UI do agente.
    fn client_folder_names(self) -> &'static [&'static str] {
        match self {
            PokerRoom::PokerStars => &[
                "PokerStars",
                "PokerStarsIT",
                "PokerStarsFR",
                "PokerStarsES",
                "PokerStarsPT",
                "PokerStars.ES",
            ],
            PokerRoom::GgPoker => &["GGPoker", "GGNetwork", "Natural8"],
            PokerRoom::PartyPoker => &["PartyGaming/PartyPoker", "partypoker"],
            PokerRoom::Poker888 => &["888poker", "888 Poker"],
            // ACR roda no cliente da Winning Poker Network — pasta de
            // instalação varia por skin (ACR, Black Chip Poker, True
            // Poker), mas hand history geralmente vai em "HH" em vez de
            // "HandHistory" (ver default_search_paths).
            PokerRoom::Acr => &["ACR Poker", "AmericasCardroom", "Americas Cardroom"],
        }
    }

    /// Nome(s) da subpasta de hand history dentro da pasta do cliente.
    /// A maioria usa "HandHistory"; a Winning Poker Network (ACR) usa "HH".
    fn history_subfolder_names(self) -> &'static [&'static str] {
        match self {
            PokerRoom::Acr => &["HH", "HandHistory"],
            _ => &["HandHistory"],
        }
    }

    /// Nome(s) da subpasta de resumo de torneio (Tournament Summary)
    /// dentro da pasta do cliente — arquivo separado da hand history, com
    /// buy-in/colocação/premiação em vez das mãos jogadas. Só o nome
    /// PokerStars ("TournamentSummary") é confirmado; as demais salas
    /// caem no mesmo nome por falta de amostra real — mesmo status
    /// "best-effort" que `client_folder_names`/`sniff` já documentam pra
    /// PartyPoker/888poker/ACR.
    fn tournament_summary_subfolder_names(self) -> &'static [&'static str] {
        match self {
            PokerRoom::PokerStars => &["TournamentSummary"],
            PokerRoom::Acr => &["TS", "TournamentSummary"],
            _ => &["TournamentSummary"],
        }
    }

    fn subfolder_names(self, kind: FileKind) -> &'static [&'static str] {
        match kind {
            FileKind::HandHistory => self.history_subfolder_names(),
            FileKind::TournamentSummary => self.tournament_summary_subfolder_names(),
        }
    }

    /// Pastas fora das pastas do usuário onde o cliente dessa sala grava
    /// por padrão. O ACR instala em `C:\ACR Poker` e grava hand history (e
    /// resumo de torneio) em `handHistory\<usuário>` ali dentro — antes o
    /// jogador precisava adicionar essa pasta à mão.
    fn fixed_roots(self) -> Vec<PathBuf> {
        match self {
            PokerRoom::Acr if cfg!(windows) => {
                let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
                vec![PathBuf::from(format!("{drive}\\")).join("ACR Poker").join("handHistory")]
            }
            _ => Vec::new(),
        }
    }

    /// Pastas onde o cliente dessa sala plausivelmente grava hand history
    /// ou resumo de torneio (`kind`), para o sistema operacional atual.
    /// Caminhos que não existem no disco são descartados por quem chama
    /// (ver `discover`), então listar candidatos "a mais" aqui é
    /// seguro.
    pub fn default_search_paths(self, kind: FileKind) -> Vec<PathBuf> {
        let mut roots = self.fixed_roots();
        let home = dirs::home_dir();
        let documents = dirs::document_dir();
        let config = dirs::config_dir(); // %APPDATA% no Windows, ~/.config no Linux
        let data_local = dirs::data_local_dir(); // %LOCALAPPDATA% no Windows

        for folder in self.client_folder_names() {
            for sub in self.subfolder_names(kind) {
                if let Some(doc) = &documents {
                    roots.push(doc.join(folder).join(sub));
                }
                if let Some(cfg) = &config {
                    roots.push(cfg.join(folder).join(sub));
                }
                if let Some(local) = &data_local {
                    roots.push(local.join(folder).join(sub));
                }
                // macOS: clientes de poker costumam gravar em Application Support.
                if let Some(h) = &home {
                    roots.push(h.join("Library/Application Support").join(folder).join(sub));
                }
            }
        }
        roots
    }

    /// Heurística barata pra reconhecer se um texto é hand history dessa
    /// sala, olhando só o início do arquivo (evita ler/parsear arquivos
    /// grandes por completo só pra descartá-los). PokerStars e GGPoker são
    /// confirmados contra hand history real (mesmos marcadores do parser em
    /// lib/poker/hand-parser.ts); PartyPoker, 888poker e ACR são best-effort
    /// — não temos amostra real ainda, então a varredura confia principalmente
    /// na pasta de origem (client_folder_names) e só usa isto como reforço.
    pub fn sniff(self, head: &str) -> bool {
        match self {
            PokerRoom::PokerStars => {
                head.contains("PokerStars Hand #") || head.contains("Mão PokerStars #")
            }
            PokerRoom::GgPoker => head.contains("GGPoker Hand") || head.contains("Poker Hand #"),
            PokerRoom::PartyPoker => {
                head.to_lowercase().contains("partypoker") || head.contains("Game #")
            }
            PokerRoom::Poker888 => head.contains("888poker") || head.contains("Game #"),
            // O resumo de torneio da ACR (JSON) também cita a rede pelo
            // nome; sem o `!is_json` ele virava "mão" e ia pro lugar errado.
            PokerRoom::Acr => {
                head.lines().any(is_wpn_hand_start)
                    || head.contains("Stage #")
                    || (mentions_wpn(head) && !is_json(head))
            }
        }
    }

    /// Mesma ideia de `sniff`, pro resumo de torneio em vez da mão. Só
    /// PokerStars é confirmado (formato documentado, "PokerStars
    /// Tournament #NNN" / "Torneio PokerStars #NNN" no cabeçalho); as
    /// demais salas são best-effort, sem amostra real — mesmo status do
    /// resto do arquivo (ver `sniff`).
    pub fn sniff_tournament_summary(self, head: &str) -> bool {
        match self {
            PokerRoom::PokerStars => {
                head.contains("PokerStars Tournament #") || head.contains("Torneio PokerStars #")
            }
            // "Tournament #" sozinho é genérico demais: só vale quando o
            // texto não fala da Winning Poker Network (resumo do ACR).
            PokerRoom::GgPoker => {
                head.contains("GGPoker Tournament") || (head.contains("Tournament #") && !mentions_wpn(head))
            }
            PokerRoom::PartyPoker => {
                head.to_lowercase().contains("partypoker") && head.to_lowercase().contains("tournament")
            }
            PokerRoom::Poker888 => head.contains("888poker") && head.to_lowercase().contains("tournament"),
            PokerRoom::Acr => mentions_wpn(head) && head.to_lowercase().contains("tournament"),
        }
    }

    pub fn sniff_kind(self, kind: FileKind, head: &str) -> bool {
        match kind {
            FileKind::HandHistory => self.sniff(head),
            FileKind::TournamentSummary => self.sniff_tournament_summary(head),
        }
    }

    /// De qual sala é o arquivo, olhando só o começo dele (`head`). Cada
    /// arquivo fica com UMA sala: primeiro a da pasta onde ele foi achado
    /// (`hint`, quando é a pasta padrão de uma sala), depois as demais na
    /// ordem de `ALL` (as confirmadas contra arquivo real primeiro).
    ///
    /// Arquivo que já começa com uma mão nunca é resumo de torneio: antes,
    /// mão de torneio do PokerStars e do ACR ("... Tournament #123 ...")
    /// caía na regra do resumo do GGPoker e era enviada como torneio do
    /// GGPoker quando a pasta de mãos estava em "Importar torneios".
    pub fn classify(kind: FileKind, head: &str, hint: Option<PokerRoom>) -> Option<PokerRoom> {
        if kind == FileKind::TournamentSummary && looks_like_hand_history(head) {
            return None;
        }
        hint.into_iter()
            .chain(PokerRoom::ALL)
            .find(|room| room.sniff_kind(kind, head))
    }
}

/// Linha que abre uma mão da Winning Poker Network (ACR) no formato atual:
/// "Hand #2134567890 - Holdem(No Limit) - $0.05/$0.10 - 2026/09/21 ...", ou
/// "Hand #... - Tournament #... - ..." em torneio. Antes o Radar só
/// procurava o nome da rede/sala, que não aparece nesse cabeçalho — e
/// todo arquivo do ACR era descartado sem aviso.
///
/// Arquivo REAL da ACR (03/10/2026) começa com "Game Hand #" ("Game Hand
/// #2838198875 - Tournament #36074377 - Holdem (No Limit) - Level 10 ..."),
/// não só "Hand #" — sem aceitar o "Game " na frente, as mãos de verdade
/// continuavam sendo descartadas.
fn is_wpn_hand_start(line: &str) -> bool {
    let l = line.trim_start_matches('\u{feff}').trim_start();
    let l = l.strip_prefix("Game ").unwrap_or(l);
    let Some(rest) = l.strip_prefix("Hand #") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(" - ")
}

/// O texto cita a Winning Poker Network ou o Americas Cardroom pelo nome.
/// O resumo de torneio real (JSON, .ots) escreve tudo junto:
/// "network_name":"WinningPokerNetwork", "site_name":"AmericasCardroom".
fn mentions_wpn(head: &str) -> bool {
    let lower = head.to_lowercase().replace(' ', "");
    lower.contains("winningpokernetwork") || lower.contains("americascardroom")
}

/// Começa com "{" — o resumo de torneio da ACR é um JSON numa linha só.
fn is_json(head: &str) -> bool {
    head.trim_start_matches('\u{feff}').trim_start().starts_with('{')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACR_CASH: &str = "Hand #2134567890 - Holdem(No Limit) - $0.05/$0.10 - 2026/09/21 16:35:41 UTC\n\
        Table 'Aurora' 6-max Seat #3 is the button\nSeat 1: Heroi ($10.00)\n";
    const ACR_TORNEIO: &str = "Hand #2134567891 - Tournament #24305619 - Holdem(No Limit) - Level 1 (10.00/20.00) - 2026/09/21 18:00:05 UTC\n\
        Table '24305619 1' 9-max Seat #1 is the button\n";
    const PS_MAO_DE_TORNEIO: &str =
        "PokerStars Hand #1234: Tournament #555, $10+$1 USD Hold'em No Limit - Level I (10/20) - 2026/09/21\n";

    #[test]
    fn recognizes_acr_hand_history() {
        for texto in [ACR_CASH, ACR_TORNEIO] {
            assert_eq!(PokerRoom::classify(FileKind::HandHistory, texto, None), Some(PokerRoom::Acr));
        }
    }

    // Começo dos arquivos REAIS da ACR (03/10/2026): mãos e resumo (.ots).
    const ACR_MAO_REAL: &str = "Game Hand #2838198875 - Tournament #36074377 - Holdem (No Limit) - Level 10 (1800.00/3600.00) - 2026/10/03 17:48:09 UTC\n\
        Table '20' 8-max Seat #7 is the button\nSeat 1: Vilao (599896.00)\n";
    const ACR_RESUMO_REAL: &str = r#"{"spec_version":"1.0.0","network_name":"WinningPokerNetwork","tournament_number":"T#36074377","start_date_utc":"2026-10-03T17:47:59Z","player_count":308,"tournament_finishes_and_winnings":[{"player_name":"Vilao","finish_position":1,"prize":0,"ticket_value":0}],"site_name":"AmericasCardroom"}"#;

    #[test]
    fn recognizes_real_acr_files() {
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, ACR_MAO_REAL, None), Some(PokerRoom::Acr));
        assert_eq!(PokerRoom::classify(FileKind::TournamentSummary, ACR_MAO_REAL, None), None);
        assert_eq!(
            PokerRoom::classify(FileKind::TournamentSummary, ACR_RESUMO_REAL, None),
            Some(PokerRoom::Acr)
        );
        // Mesmo achado na pasta da ACR, o resumo não é mão.
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, ACR_RESUMO_REAL, Some(PokerRoom::Acr)), None);
    }

    #[test]
    fn hand_history_is_never_a_tournament_summary() {
        for texto in [ACR_CASH, ACR_TORNEIO, PS_MAO_DE_TORNEIO] {
            assert_eq!(PokerRoom::classify(FileKind::TournamentSummary, texto, None), None);
        }
    }

    #[test]
    fn real_tournament_summaries_still_recognized() {
        let ps = "PokerStars Tournament #1234567890, No Limit Hold'em\nBuy-In: $10.00+$1.00\n";
        assert_eq!(
            PokerRoom::classify(FileKind::TournamentSummary, ps, None),
            Some(PokerRoom::PokerStars)
        );
        let gg = "Tournament #123456789, Bounty Hunters $10, Hold'em No Limit\nBuy-in: $9.6+$0.4\n";
        assert_eq!(
            PokerRoom::classify(FileKind::TournamentSummary, gg, None),
            Some(PokerRoom::GgPoker)
        );
        let acr = "Americas Cardroom Tournament #24305619\nBuy-In: $0.95 + $0.05\n";
        assert_eq!(PokerRoom::classify(FileKind::TournamentSummary, acr, None), Some(PokerRoom::Acr));
    }

    #[test]
    fn folder_hint_wins_over_order() {
        // "Game #" casa com PartyPoker e com 888poker; na pasta do 888, é 888.
        let texto = "Game #123 starts.\n";
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, texto, None), Some(PokerRoom::PartyPoker));
        assert_eq!(
            PokerRoom::classify(FileKind::HandHistory, texto, Some(PokerRoom::Poker888)),
            Some(PokerRoom::Poker888)
        );
    }

    #[test]
    fn other_rooms_are_not_taken_for_acr() {
        let ps = "PokerStars Hand #1: Hold'em No Limit ($0.01/$0.02) - 2026/09/21\n";
        let gg = "Poker Hand #HD1: Hold'em No Limit ($0.01/$0.02) - 2026/09/21\n";
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, ps, None), Some(PokerRoom::PokerStars));
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, gg, None), Some(PokerRoom::GgPoker));
        assert!(!is_wpn_hand_start("Hand #abc - x"));
        assert!(!is_wpn_hand_start("Hand #123: x"));
    }
}
