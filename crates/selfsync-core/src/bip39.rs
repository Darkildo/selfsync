//! Фраза восстановления: 32 байта энтропии ↔ 24 слова BIP-39 (английский словарь,
//! 8 бит контрольной суммы — первый байт SHA-256 энтропии). Своя реализация: крейт
//! `bip39` тянет лишнее, а в WASM считается каждый килобайт.

use sha2::{Digest, Sha256};

/// Официальный словарь BIP-39 (english.txt, SHA-256 2f5eed53…dbda), 2048 слов.
const WORDLIST: &str = include_str!("bip39-english.txt");
/// Слов во фразе для 256 бит.
pub const WORDS: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhraseError {
    #[error("во фразе {0} слов, нужно 24")]
    WordCount(usize),
    /// Номер слова — с единицы, как на листке с фразой.
    #[error("слово {index} («{word}») не из словаря BIP-39")]
    UnknownWord { index: usize, word: String },
    #[error("контрольная сумма не сходится: слова перепутаны или одно заменено похожим")]
    Checksum,
}

fn words() -> impl Iterator<Item = &'static str> {
    WORDLIST.lines().map(str::trim).filter(|w| !w.is_empty())
}

fn word_at(i: usize) -> &'static str {
    words().nth(i).unwrap_or("?")
}

fn index_of(w: &str) -> Option<usize> {
    // Словарь отсортирован: двоичный поиск без отдельной таблицы.
    let list: Vec<&str> = words().collect();
    list.binary_search(&w).ok()
}

/// 24 слова для 32 байт.
pub fn encode(entropy: &[u8; 32]) -> Vec<String> {
    let checksum = Sha256::digest(entropy)[0];
    let mut bits = Vec::with_capacity(264);
    for b in entropy.iter().chain(std::iter::once(&checksum)) {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
    bits.chunks(11)
        .map(|c| {
            let idx = c.iter().fold(0usize, |acc, b| (acc << 1) | usize::from(*b));
            word_at(idx).to_owned()
        })
        .collect()
}

/// 32 байта из фразы (слова через пробелы или переводы строк, регистр любой).
pub fn decode(phrase: &str) -> Result<[u8; 32], PhraseError> {
    let ws: Vec<String> = phrase.split_whitespace().map(str::to_lowercase).collect();
    if ws.len() != WORDS {
        return Err(PhraseError::WordCount(ws.len()));
    }
    let mut bits = Vec::with_capacity(264);
    for (n, w) in ws.iter().enumerate() {
        let Some(idx) = index_of(w) else {
            return Err(PhraseError::UnknownWord {
                index: n + 1,
                word: w.clone(),
            });
        };
        for i in (0..11).rev() {
            bits.push(u8::from((idx >> i) & 1 == 1));
        }
    }
    let bytes: Vec<u8> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0u8, |acc, b| (acc << 1) | b))
        .collect();
    let mut entropy = [0u8; 32];
    entropy.copy_from_slice(&bytes[..32]);
    if Sha256::digest(entropy)[0] != bytes[32] {
        return Err(PhraseError::Checksum);
    }
    Ok(entropy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn wordlist_is_the_official_one() {
        let list: Vec<&str> = words().collect();
        assert_eq!(list.len(), 2048);
        assert_eq!((list[0], list[2047]), ("abandon", "zoo"));
        let mut sorted = list.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, list, "отсортирован (двоичный поиск)");
        let hex: String = Sha256::digest(WORDLIST.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda"
        );
    }

    /// Тестовые векторы BIP-39 (256 бит, vectors.json из спецификации).
    #[test]
    fn spec_vectors() {
        let vectors = [
            (
                "0000000000000000000000000000000000000000000000000000000000000000",
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art",
            ),
            (
                "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
                "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title",
            ),
            (
                "8080808080808080808080808080808080808080808080808080808080808080",
                "letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic bless",
            ),
            (
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote",
            ),
            (
                "68a79eaca2324873eacc50cb9c6eca8cc68ea5d936f98787c60c7ebc74e6ce7c",
                "hamster diagram private dutch cause delay private meat slide toddler razor book happy fancy gospel tennis maple dilemma loan word shrug inflict delay length",
            ),
            (
                "9f6a2878b2520799a44ef18bc7df394e7061a224d2c33cd015b157d746869863",
                "panda eyebrow bullet gorilla call smoke muffin taste mesh discover soft ostrich alcohol speed nation flash devote level hobby quick inner drive ghost inside",
            ),
        ];
        for (hex, phrase) in vectors {
            let e = hex32(hex);
            assert_eq!(encode(&e).join(" "), phrase, "{hex}");
            assert_eq!(decode(phrase).unwrap(), e, "{hex}");
        }
    }

    #[test]
    fn errors_name_the_word() {
        let good = encode(&hex32(
            "68a79eaca2324873eacc50cb9c6eca8cc68ea5d936f98787c60c7ebc74e6ce7c",
        ));
        // опечатка в 5-м слове
        let mut typo = good.clone();
        typo[4] = "causee".into();
        assert_eq!(
            decode(&typo.join(" ")),
            Err(PhraseError::UnknownWord {
                index: 5,
                word: "causee".into()
            })
        );
        // словарное, но не то слово — контрольная сумма
        let mut swapped = good.clone();
        swapped[23] = if swapped[23] == "abandon" {
            "ability".into()
        } else {
            "abandon".into()
        };
        assert_eq!(decode(&swapped.join(" ")), Err(PhraseError::Checksum));
        assert_eq!(decode("abandon zoo"), Err(PhraseError::WordCount(2)));
        // регистр и переводы строк не мешают
        assert!(decode(&good.join("\n").to_uppercase()).is_ok());
    }
}
