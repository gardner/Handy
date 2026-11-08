//! CMU Pronouncing Dictionary lookups, for names and words misaki's own
//! lexicon lacks ("Gardner", "Nguyen", "Zuckerberg").
//!
//! `cmudict.txt.gz` is cmusphinx/cmudict (BSD-2-Clause, see `CMUDICT-LICENSE`),
//! reduced to each word's first pronunciation. It's ~850 KB compressed and only
//! unpacked the first time an unknown word comes up.

use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::io::Read;

static DICT: Lazy<HashMap<String, String>> = Lazy::new(|| {
    let mut text = String::new();
    let packed: &[u8] = include_bytes!("cmudict.txt.gz");
    if let Err(e) = GzDecoder::new(packed).read_to_string(&mut text) {
        log::error!("Couldn't unpack the CMU dictionary: {e}");
        return HashMap::new();
    }
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(word, arpabet)| (word.to_string(), arpabet.to_string()))
        .collect()
});

/// The word's pronunciation in misaki's phoneme alphabet, if CMUdict has it.
pub fn lookup(word: &str) -> Option<String> {
    DICT.get(&word.to_lowercase()).map(|a| arpabet_to_misaki(a))
}

/// Convert space-separated ARPAbet ("G AA1 R D N ER0") to misaki/Kokoro
/// phonemes ("ɡˈɑɹdnəɹ"). Stress marks go right before the stressed vowel,
/// as misaki writes them.
pub fn arpabet_to_misaki(arpabet: &str) -> String {
    let mut out = String::new();
    for phone in arpabet.split_whitespace() {
        let (base, stress) = match phone.as_bytes().last() {
            Some(b'0') => (&phone[..phone.len() - 1], Some(0)),
            Some(b'1') => (&phone[..phone.len() - 1], Some(1)),
            Some(b'2') => (&phone[..phone.len() - 1], Some(2)),
            _ => (phone, None),
        };
        match stress {
            Some(1) => out.push('ˈ'),
            Some(2) => out.push('ˌ'),
            _ => {}
        }
        let unstressed = stress == Some(0);
        out.push_str(match base {
            "AA" => "ɑ",
            "AE" => "æ",
            "AH" if unstressed => "ə",
            "AH" => "ʌ",
            "AO" => "ɔ",
            "AW" => "W",
            "AY" => "I",
            "EH" => "ɛ",
            "ER" if unstressed => "əɹ",
            "ER" => "ɜɹ",
            "EY" => "A",
            "IH" => "ɪ",
            "IY" => "i",
            "OW" => "O",
            "OY" => "Y",
            "UH" => "ʊ",
            "UW" => "u",
            "B" => "b",
            "CH" => "ʧ",
            "D" => "d",
            "DH" => "ð",
            "F" => "f",
            "G" => "ɡ",
            "HH" => "h",
            "JH" => "ʤ",
            "K" => "k",
            "L" => "l",
            "M" => "m",
            "N" => "n",
            "NG" => "ŋ",
            "P" => "p",
            "R" => "ɹ",
            "S" => "s",
            "SH" => "ʃ",
            "T" => "t",
            "TH" => "θ",
            "V" => "v",
            "W" => "w",
            "Y" => "j",
            "Z" => "z",
            "ZH" => "ʒ",
            _ => "",
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_arpabet() {
        assert_eq!(arpabet_to_misaki("G AA1 R D N ER0"), "ɡˈɑɹdnəɹ");
        assert_eq!(arpabet_to_misaki("HH AE1 N D IY0"), "hˈændi");
    }

    #[test]
    fn finds_names() {
        assert_eq!(lookup("Gardner").as_deref(), Some("ɡˈɑɹdnəɹ"));
        assert!(lookup("qzxwv").is_none());
    }
}
