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
    /// grandes por completo só pra descartá-los). PokerStars, GGPoker e ACR
    /// são confirmados contra hand history real (mesmos marcadores do parser
    /// em lib/poker/hand-parser.ts); PartyPoker e 888poker são best-effort
    /// (formato clássico das duas, sem amostra real ainda).
    ///
    /// Sempre exige o CABEÇALHO DE MÃO no começo de uma linha, no formato
    /// inteiro da sala (marcador + número + o separador que vem depois) —
    /// antes bastava o texto conter "Game #" ou "Poker Hand #" em qualquer
    /// lugar dos primeiros 4 KB, e qualquer .txt de uma pasta extra que
    /// citasse "Game #12" virava "mão do PartyPoker" e era enviado pro site.
    pub fn sniff(self, head: &str) -> bool {
        match self {
            PokerRoom::PokerStars => any_line(head, is_pokerstars_hand_start),
            PokerRoom::GgPoker => any_line(head, is_gg_hand_start),
            // "#Game No : N" abre a mão tanto no PartyPoker quanto no
            // 888poker; com a linha própria do 888 no arquivo, é do 888.
            PokerRoom::PartyPoker => {
                any_line(head, is_party_hand_start)
                    || (any_line(head, is_game_no_line) && !any_line(head, is_888_hand_start))
            }
            PokerRoom::Poker888 => any_line(head, is_888_hand_start) || any_line(head, is_game_no_line),
            // Só o cabeçalho da mão: o nome da rede solto no texto (que o
            // resumo de torneio .ots e qualquer anotação também têm) não
            // conta mais — a mão real da ACR nem traz o nome (ver
            // `is_wpn_hand_start`).
            PokerRoom::Acr => any_line(head, is_wpn_hand_start) || any_line(head, is_wpn_stage_start),
        }
    }

    /// Mesma ideia de `sniff`, pro resumo de torneio em vez da mão. Só
    /// PokerStars, GGPoker e o .ots da ACR são confirmados; PartyPoker e
    /// 888poker são best-effort, sem amostra real — mesmo status do resto
    /// do arquivo (ver `sniff`). Também exige o cabeçalho no começo da
    /// linha, não o texto solto em qualquer lugar.
    pub fn sniff_tournament_summary(self, head: &str) -> bool {
        match self {
            PokerRoom::PokerStars => any_line(head, |l| {
                starts_with_number(l, "PokerStars Tournament #") || starts_with_number(l, "Torneio PokerStars #")
            }),
            // "Tournament #123456789, Nome do torneio, Hold'em No Limit" —
            // e o texto não pode falar da Winning Poker Network (resumo do
            // ACR em texto também tem "Tournament #").
            PokerRoom::GgPoker => {
                any_line(head, |l| {
                    l.starts_with("GGPoker Tournament #") || number_then(l, "Tournament #", ",")
                }) && !mentions_wpn(head)
            }
            PokerRoom::PartyPoker => any_line(head, |l| room_tournament_line(l, &["partypoker"])),
            PokerRoom::Poker888 => any_line(head, |l| room_tournament_line(l, &["888poker"])),
            // Real: JSON (.ots) com "network_name":"WinningPokerNetwork".
            // Em texto, uma linha que começa com o nome da sala/rede.
            PokerRoom::Acr => {
                (is_json(head) && mentions_wpn(head) && head.to_lowercase().contains("tournament"))
                    || any_line(head, |l| {
                        room_tournament_line(l, &["americascardroom", "winningpokernetwork"])
                    })
            }
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

/// Alguma linha do começo do arquivo (sem BOM e espaços à esquerda) passa
/// no teste.
fn any_line(head: &str, test: impl Fn(&str) -> bool) -> bool {
    head.lines()
        .map(|line| line.trim_start_matches('\u{feff}').trim_start())
        .any(test)
}

/// `line` começa com `prefix` seguido de pelo menos um dígito, e o que
/// vem depois dos dígitos começa com `after`.
fn number_then(line: &str, prefix: &str, after: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(after)
}

/// `line` começa com `prefix` seguido de pelo menos um dígito.
fn starts_with_number(line: &str, prefix: &str) -> bool {
    number_then(line, prefix, "")
}

/// "PokerStars Hand #250000000001: Tournament #..." (e as variantes Zoom,
/// "Game" antiga e o client em português "Mão PokerStars #...:") — mesma
/// lista de `text::is_hand_start`, sempre com número e ":" depois.
fn is_pokerstars_hand_start(line: &str) -> bool {
    ["PokerStars Hand #", "PokerStars Zoom Hand #", "PokerStars Game #", "Mão PokerStars #"]
        .iter()
        .any(|prefix| number_then(line, prefix, ":"))
}

/// "Poker Hand #TM6498385516: Tournament #316315080, ..." — o número da
/// mão do GGPoker tem letras na frente (TM, HD, RC...), então aceita
/// letras e dígitos até o ":".
fn is_gg_hand_start(line: &str) -> bool {
    ["Poker Hand #", "GGPoker Hand #"].iter().any(|prefix| {
        let Some(rest) = line.strip_prefix(prefix) else {
            return false;
        };
        let id = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
        id > 0 && rest[id..].starts_with(':')
    })
}

/// Formatos do PartyPoker: o clássico ("Game #123 starts." e "***** Hand
/// History for Game 123 *****") e o da amostra do site ("Game hand #123 -
/// Tournament #..."; com "h" minúsculo — o "Game Hand #" maiúsculo é da
/// ACR).
fn is_party_hand_start(line: &str) -> bool {
    number_then(line, "Game #", " starts.")
        || number_then(line, "***** Hand History for Game ", " *****")
        || number_then(line, "Game hand #", " - ")
}

/// "***** 888poker Hand History for Game 123 *****" (ou "888poker Snap
/// Poker Hand History ..." nas mesas rápidas).
fn is_888_hand_start(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("***** 888poker ") else {
        return false;
    };
    rest.find("Hand History for Game ")
        .is_some_and(|i| number_then(&rest[i..], "Hand History for Game ", " *****"))
}

/// "#Game No : 123" — primeira linha da mão no PartyPoker e no 888poker.
fn is_game_no_line(line: &str) -> bool {
    starts_with_number(line, "#Game No : ")
}

/// Formato antigo da rede: "Stage #123: Holdem ...".
fn is_wpn_stage_start(line: &str) -> bool {
    number_then(line, "Stage #", ":")
}

/// Linha de resumo de torneio de uma sala sem amostra real: começa com o
/// nome da sala (sem diferenciar maiúscula e ignorando espaços) e fala de
/// "Tournament #" na mesma linha.
fn room_tournament_line(line: &str, names: &[&str]) -> bool {
    let compact = line.to_lowercase().replace(' ', "");
    names.iter().any(|name| compact.starts_with(name)) && compact.contains("tournament#")
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
        // "#Game No :" casa com PartyPoker e com 888poker; na pasta do 888, é 888.
        let texto = "#Game No : 123\n";
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

    // Começo de mãos REAIS (tests/maos.ts, maos-gg.ts e maos-acr.ts no
    // repositório do site): nada disso pode deixar de ser reconhecido.
    const PS_REAL_EN: &str = "PokerStars Hand #250000000001: Tournament #3000000001, $10+$1 USD Hold'em No Limit - Level I (10/20) - 2026/09/20 20:00:01 ET\n\
        Table '3000000001 1' 9-max Seat #1 is the button\nSeat 1: Vilao (3000 in chips)\n";
    const PS_REAL_PT: &str = "Mão PokerStars #250000000001: Torneio #3900000001, $ 7,50+$ 7,50+$ 1,50 USD Hold'em No Limit - Nível I (10/20) - 20/09/2026 20:00:00 BRT\n\
        Mesa '3900000001 1' 9-max Lugar #1 é o botão\n";
    const GG_REAL: &str = "Poker Hand #TM6498385516: Tournament #316315080, Mini Forty Stack $4.40 Hold'em No Limit - Level16(2,000/4,000(500)) - 2026/10/05 14:18:12\n\
        Table '137' 8-max Seat #7 is the button\nSeat 1: 1003ed72 (50,412 in chips)\n";

    #[test]
    fn real_hand_histories_still_classify() {
        let casos = [
            (PS_REAL_EN, PokerRoom::PokerStars),
            (PS_REAL_PT, PokerRoom::PokerStars),
            (GG_REAL, PokerRoom::GgPoker),
            (ACR_MAO_REAL, PokerRoom::Acr),
            (ACR_CASH, PokerRoom::Acr),
        ];
        for (texto, sala) in casos {
            assert_eq!(PokerRoom::classify(FileKind::HandHistory, texto, None), Some(sala), "{texto}");
            // Com BOM e depois de linhas em branco também.
            let com_bom = format!("\u{feff}\n\n{texto}");
            assert_eq!(PokerRoom::classify(FileKind::HandHistory, &com_bom, None), Some(sala), "{texto}");
            // A pasta padrão de outra sala não muda o resultado.
            assert_eq!(
                PokerRoom::classify(FileKind::HandHistory, texto, Some(PokerRoom::Poker888)),
                Some(sala),
                "{texto}"
            );
        }
    }

    #[test]
    fn classic_party_and_888_headers_classify() {
        let party = "#Game No : 17469349023\n***** Hand History for Game 17469349023 *****\n$2 USD NL Texas Hold'em - Sunday, February 12, 20:21:05 CET 2017\n";
        let party_antiga = "Game #5014561616 starts.\n\n#Game No : 5014561616\n***** Hand History for Game 5014561616 *****\n";
        let party_site = "Game hand #1054329871 - Tournament #3482019 - Holdem(No Limit) - Level 1\n";
        let o888 = "#Game No : 1046488393\n***** 888poker Hand History for Game 1046488393 *****\n$0.01/$0.02 Blinds No Limit Holdem - *** 21 02 2020 14:11:04\n";
        for texto in [party, party_antiga, party_site] {
            assert_eq!(
                PokerRoom::classify(FileKind::HandHistory, texto, None),
                Some(PokerRoom::PartyPoker),
                "{texto}"
            );
        }
        assert_eq!(PokerRoom::classify(FileKind::HandHistory, o888, None), Some(PokerRoom::Poker888));
    }

    #[test]
    fn text_that_only_mentions_poker_markers_is_not_a_hand() {
        let textos = [
            // O caso que motivou o aperto: "Game #12" no meio de um texto.
            "Anotações da semana\nReview do Game #12 com o coach\n",
            "Game #12 foi o melhor jogo do campeonato\n",
            "Lista de compras\nGame #12, Poker Hand #3 e PokerStars Hand #4 citados no meio da linha\n",
            "Vi um vídeo: PokerStars Hand #sem-numero\n",
            "Poker Hand # sem id\nGGPoker Hand history export\n",
            "Hoje joguei no 888poker e no partypoker.\nAmericas Cardroom e Winning Poker Network também.\n",
            "Stage # sem número\n",
        ];
        for texto in textos {
            for kind in [FileKind::HandHistory, FileKind::TournamentSummary] {
                assert_eq!(PokerRoom::classify(kind, texto, None), None, "{kind:?}: {texto}");
                for hint in PokerRoom::ALL {
                    assert_eq!(PokerRoom::classify(kind, texto, Some(hint)), None, "{kind:?}/{hint:?}: {texto}");
                }
            }
        }
    }

    #[test]
    fn summary_needs_header_at_line_start() {
        let solto = "Notas: o Tournament #123, aquele do domingo, e o PokerStars Tournament #9 foram bons\n";
        assert_eq!(PokerRoom::classify(FileKind::TournamentSummary, solto, None), None);
        let pt = "Torneio PokerStars #3900000001, Hold'em No Limit\nBuy-In: $ 7,50+$ 7,50+$ 1,50\n";
        assert_eq!(
            PokerRoom::classify(FileKind::TournamentSummary, pt, None),
            Some(PokerRoom::PokerStars)
        );
    }
}
