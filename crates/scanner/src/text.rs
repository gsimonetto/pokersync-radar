//! Texto dos arquivos de hand history: leitura em qualquer codificação,
//! divisão em partes nos limites de mão e "só o que é novo" num arquivo
//! que cresceu desde o último envio.
//!
//! Por que existe: `read_to_string` só aceita UTF-8 — arquivo salvo em
//! UTF-16 (com BOM) ou em Windows-1252/Latin-1 (acentos de nome de
//! jogador, cliente antigo) era pulado sem aviso nenhum. E o servidor do
//! PokerSync (Vercel) aceita no máximo ~4,5 MB por envio e 5 MB por
//! arquivo, enquanto a varredura aceita arquivos de até 20 MB — então um
//! histórico grande precisa ir em partes, sempre cortadas no começo de uma
//! mão (o backend separa as mãos pelo marcador de início, ver
//! `splitHands` em lib/poker/hand-parser.ts no produto).

/// Decodifica o conteúdo bruto de um arquivo: UTF-8 (com ou sem BOM),
/// UTF-16 LE/BE (com BOM) e, quando não é UTF-8 válido, Windows-1252 —
/// superconjunto do Latin-1 usado por clientes antigos no Windows.
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return decode_utf8_or_1252(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    decode_utf8_or_1252(bytes)
}

/// Versão pra "cheirar" só o começo do arquivo: o corte em 4 KB pode cair
/// no meio de um caractere, então aqui nunca cai pro Windows-1252 por
/// causa disso — os marcadores procurados são todos ASCII.
pub fn decode_prefix(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(rest).into_owned()
}

fn decode_utf8_or_1252(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| windows_1252(b)).collect(),
    }
}

fn decode_utf16(bytes: &[u8], to_u16: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| to_u16([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

fn windows_1252(b: u8) -> char {
    // Só a faixa 0x80–0x9F difere do Latin-1; os 5 bytes indefinidos no
    // Windows-1252 (0x81, 0x8D, 0x8F, 0x90, 0x9D) ficam com o mesmo
    // código do Latin-1, como o próprio Windows faz.
    const FAIXA_80_9F: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => FAIXA_80_9F[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// Linha que abre uma mão nova, nos formatos que o backend reconhece
/// (mesma lista de `splitHands`/`detectSite` em lib/poker/hand-parser.ts)
/// mais os da ACR/Winning Poker Network.
pub fn is_hand_start(line: &str) -> bool {
    const MARCADORES: [&str; 10] = [
        "PokerStars Hand #",
        "PokerStars Zoom Hand #",
        "PokerStars Game #",
        "Mão PokerStars #",
        "Poker Hand #",
        "GGPoker Hand",
        "Game Hand #",
        "Game hand #",
        "***** 888poker Hand History",
        "Hand #",
    ];
    let l = line.trim_start_matches('\u{feff}').trim_start();
    MARCADORES.iter().any(|m| l.starts_with(m))
}

/// Posições (em bytes) onde dá pra cortar o texto sem partir uma mão: o
/// começo de cada linha que abre mão. Quando o formato não tem marcador
/// conhecido, vale também o começo de uma linha logo depois de uma linha
/// em branco (as salas separam as mãos com linhas em branco).
fn cut_points(text: &str) -> Vec<usize> {
    let mut by_marker = Vec::new();
    let mut by_blank = Vec::new();
    let mut offset = 0usize;
    let mut previous_blank = false;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if is_hand_start(content) {
            by_marker.push(offset);
        }
        if previous_blank && !content.trim().is_empty() {
            by_blank.push(offset);
        }
        previous_blank = content.trim().is_empty();
        offset += line.len();
    }
    if by_marker.is_empty() {
        by_blank
    } else {
        by_marker
    }
}

/// Divide `text` em partes de até `max_bytes` (bytes UTF-8), sempre
/// cortando no começo de uma mão. Só quando uma mão sozinha passa do
/// limite (não acontece com hand history de verdade) o corte cai no fim de
/// uma linha qualquer.
pub fn split_into_parts(text: &str, max_bytes: usize) -> Vec<&str> {
    let max_bytes = max_bytes.max(1);
    if text.len() <= max_bytes {
        return vec![text];
    }
    let points = cut_points(text);
    let mut parts = Vec::new();
    let mut start = 0usize;
    while text.len() - start > max_bytes {
        let limit = start + max_bytes;
        // Último ponto de corte que cabe nesta parte (e que não é o próprio começo).
        let cut = points
            .iter()
            .copied()
            .rev()
            .find(|&p| p > start && p <= limit)
            .unwrap_or_else(|| last_line_end(text, start, limit));
        parts.push(&text[start..cut]);
        start = cut;
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// Fim da última linha inteira antes de `limit` (ou o próprio `limit`,
/// ajustado pra não cortar um caractere ao meio, se não houver quebra de
/// linha nenhuma no trecho).
fn last_line_end(text: &str, start: usize, limit: usize) -> usize {
    let mut limit = limit.min(text.len());
    while limit > start && !text.is_char_boundary(limit) {
        limit -= 1;
    }
    if let Some(i) = text[start..limit].rfind('\n') {
        return start + i + 1;
    }
    if limit > start {
        return limit;
    }
    // Nem um caractere inteiro cabe no limite: avança um, pra nunca travar.
    start + text[start..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Parte "nova" de um arquivo que cresceu: tudo a partir da mão que
/// estava sendo escrita no fim da última leitura (`previous_len` bytes).
/// Reenvia essa última mão de propósito — se ela ainda estava pela metade
/// no envio anterior, agora vai inteira (o backend descarta repetidas).
pub fn new_part_since(text: &str, previous_len: usize) -> &str {
    if previous_len == 0 || previous_len > text.len() {
        return text;
    }
    let start = cut_points(text)
        .into_iter()
        .rev()
        .find(|&p| p < previous_len)
        .unwrap_or(0);
    &text[start..]
}

/// Dias desde 1970-01-01 pra uma data do calendário (algoritmo de Howard
/// Hinnant) — sem dependência de biblioteca de datas só pra isso.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverso de `days_from_civil`: (ano, mês, dia).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Primeira data "AAAA/MM/DD" (ou "AAAA-MM-DD") de uma linha, em dias
/// desde 1970. É o formato das linhas de início de mão da PokerStars e da
/// GGPoker e do "Tournament started" do resumo de torneio.
pub fn day_in_line(line: &str) -> Option<i64> {
    let b = line.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let num = |from: usize, len: usize| line[from..from + len].parse::<u32>().ok();
    for i in 0..=b.len() - 10 {
        let sep = b[i + 4];
        if (sep == b'/' || sep == b'-')
            && b[i + 7] == sep
            && (0..4).all(|k| digit(i + k))
            && digit(i + 5)
            && digit(i + 6)
            && digit(i + 8)
            && digit(i + 9)
        {
            let (year, month, day) = (num(i, 4)?, num(i + 5, 2)?, num(i + 8, 2)?);
            if (1990..=2100).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day) {
                return Some(days_from_civil(year as i64, month, day));
            }
        }
    }
    None
}

/// Data (dias desde 1970) de cada mão do texto, na ordem — `None` quando
/// a linha de início (ou a seguinte) não tem data num formato conhecido.
pub fn hand_days(text: &str) -> Vec<Option<i64>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_hand_start(line) {
            out.push(day_in_line(line).or_else(|| lines.get(i + 1).and_then(|l| day_in_line(l))));
        }
    }
    out
}

/// Impressão digital estável (FNV-1a 64 bits) — usada pra saber se o
/// começo do arquivo continua igual ao que já foi enviado. Não é
/// criptografia; só precisa ser igual entre execuções e versões do app.
pub fn fingerprint(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand(n: u32) -> String {
        format!(
            "PokerStars Hand #{n}: Tournament #1, $10+$1 USD Hold'em No Limit\nSeat 1: hero (1500 in chips)\n*** SUMMARY ***\nTotal pot 60\n\n\n"
        )
    }

    #[test]
    fn decodes_utf8_with_and_without_bom() {
        assert_eq!(decode_text("Mão".as_bytes()), "Mão");
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice("Mão".as_bytes());
        assert_eq!(decode_text(&with_bom), "Mão");
    }

    #[test]
    fn decodes_utf16_le_and_be() {
        let mut le = vec![0xFF, 0xFE];
        for u in "Mão #1".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_text(&le), "Mão #1");

        let mut be = vec![0xFE, 0xFF];
        for u in "João".encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(decode_text(&be), "João");
    }

    #[test]
    fn falls_back_to_windows_1252() {
        // "João" em Latin-1 + aspas curvas do Windows-1252 (0x93/0x94).
        let bytes = [b'J', b'o', 0xE3, b'o', b' ', 0x93, b'x', 0x94];
        assert_eq!(decode_text(&bytes), "João “x”");
    }

    #[test]
    fn prefix_decoding_tolerates_cut_characters() {
        let bytes = "PokerStars Hand #1: Mã".as_bytes();
        let cut = &bytes[..bytes.len() - 1]; // corta o "ã" no meio
        assert!(decode_prefix(cut).starts_with("PokerStars Hand #1"));
    }

    #[test]
    fn small_text_is_not_split() {
        let text = hand(1);
        assert_eq!(split_into_parts(&text, 10_000), vec![text.as_str()]);
    }

    #[test]
    fn splits_only_at_hand_starts_and_keeps_everything() {
        let text: String = (1..=50).map(hand).collect();
        let parts = split_into_parts(&text, 1_000);
        assert!(parts.len() > 1);
        assert_eq!(parts.concat(), text);
        for p in &parts {
            assert!(p.len() <= 1_000, "parte com {} bytes", p.len());
            assert!(p.starts_with("PokerStars Hand #"), "parte não começa numa mão: {:?}", &p[..30]);
        }
    }

    #[test]
    fn splits_at_blank_lines_when_format_is_unknown() {
        let block = "Linha A de uma mão desconhecida\nLinha B\n\n";
        let text = block.repeat(40);
        let parts = split_into_parts(&text, 200);
        assert_eq!(parts.concat(), text);
        for p in &parts {
            assert!(p.len() <= 200);
            assert!(p.starts_with("Linha A"));
        }
    }

    #[test]
    fn oversized_single_hand_still_splits_without_losing_text() {
        let text = format!("PokerStars Hand #1: x\n{}", "linha comprida\n".repeat(100));
        let parts = split_into_parts(&text, 100);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| !p.is_empty() && p.len() <= 100));
    }

    #[test]
    fn new_part_resends_last_hand_and_everything_after() {
        let old: String = (1..=3).map(hand).collect();
        let grown = format!("{old}{}{}", hand(4), hand(5));
        let part = new_part_since(&grown, old.len());
        assert!(part.starts_with("PokerStars Hand #3:"));
        assert!(part.ends_with(&hand(5)));
    }

    #[test]
    fn new_part_falls_back_to_whole_text() {
        let text: String = (1..=2).map(hand).collect();
        assert_eq!(new_part_since(&text, 0), text);
        assert_eq!(new_part_since(&text, text.len() + 10), text);
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 9, 25), 20721);
        for dias in [-1000, 0, 59, 60, 11016, 20721, 30000] {
            let (y, m, d) = civil_from_days(dias);
            assert_eq!(days_from_civil(y, m, d), dias);
        }
    }

    #[test]
    fn reads_hand_dates_from_start_lines() {
        let texto = format!(
            "{}{}Poker Hand #RC1: Tournament #9 - 2025/12/31 23:59:59\nx\n\n",
            hand(1),
            "PokerStars Hand #2: Tournament #1 - 2026/09/20 10:00:00 BRT [2026/09/20 9:00:00 ET]\nSeat 1: x\n\n\n"
        );
        let dias = hand_days(&texto);
        assert_eq!(dias.len(), 3);
        assert_eq!(dias[0], None); // a mão de teste não tem data
        assert_eq!(dias[1], Some(days_from_civil(2026, 9, 20)));
        assert_eq!(dias[2], Some(days_from_civil(2025, 12, 31)));
        assert_eq!(day_in_line("Tournament started 2026-01-05 18:00:00"), Some(days_from_civil(2026, 1, 5)));
        assert_eq!(day_in_line("sem data 12/34/5678"), None);
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint(""), 0xcbf29ce484222325);
    }
}
