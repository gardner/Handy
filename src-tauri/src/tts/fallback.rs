//! Pronunciation for words misaki's dictionary doesn't know (names, product
//! names, jargon). Without a fallback misaki drops them, so "Kubernetes runs"
//! would be read as just "runs".
//!
//! Strategy, in order:
//! 1. Split the word into dictionary words ("Zustand" → "zus" + "tand" style
//!    guesses work well for compounds like "hotkeys" or "dashboard").
//! 2. Spell it out letter by letter ("qzxwv" → "Q Z X W V").
//!
//! This is deliberately simple and dependency-free (MIT, no espeak-ng). A
//! neural G2P could replace it later behind the same `Fallback` trait.

use misaki_rs::{fallback::FallbackError, Fallback, Language, G2P};
use std::collections::HashMap;
use std::sync::Mutex;

/// Pieces shorter than this are too ambiguous to pronounce as words.
const MIN_PIECE: usize = 3;
/// Longer words are spelled rather than searched (cost grows with length²).
const MAX_SEGMENT_LEN: usize = 24;

pub struct SegmentingFallback {
    /// A fallback-free G2P used to look words up.
    lookup: G2P,
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl SegmentingFallback {
    pub fn new(lang: Language) -> Self {
        Self {
            lookup: G2P::with_fallback(lang, None),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Phonemes for `word` if the dictionary knows it as a single word.
    fn known(&self, word: &str) -> Option<String> {
        if let Some(hit) = self.cache.lock().unwrap().get(word) {
            return hit.clone();
        }
        let result = self.lookup.g2p(word).ok().and_then(|(phonemes, tokens)| {
            let resolved = tokens
                .iter()
                .all(|t| t.phonemes.is_some() || t.text.trim().is_empty());
            (resolved && !phonemes.trim().is_empty()).then(|| phonemes.trim().to_string())
        });
        self.cache
            .lock()
            .unwrap()
            .insert(word.to_string(), result.clone());
        result
    }

    /// Fewest dictionary words that make up `word`, joined as phonemes.
    fn segment(&self, word: &str) -> Option<String> {
        let chars: Vec<char> = word.chars().collect();
        let n = chars.len();
        if !(MIN_PIECE * 2..=MAX_SEGMENT_LEN).contains(&n) {
            return None;
        }
        // best[i] = (piece count, phonemes) for chars[..i]
        let mut best: Vec<Option<(usize, String)>> = vec![None; n + 1];
        best[0] = Some((0, String::new()));
        for end in MIN_PIECE..=n {
            for start in 0..=end - MIN_PIECE {
                let Some((count, prefix)) = best[start].clone() else {
                    continue;
                };
                if best[end].as_ref().is_some_and(|(c, _)| *c <= count + 1) {
                    continue;
                }
                let piece: String = chars[start..end].iter().collect();
                if let Some(p) = self.known(&piece) {
                    // Stress marks inside a compound sound choppy; keep only
                    // the first piece's primary stress.
                    let p = if count == 0 { p } else { p.replace('ˈ', "ˌ") };
                    best[end] = Some((count + 1, format!("{prefix}{p}")));
                }
            }
        }
        best[n].take().map(|(_, p)| p)
    }

    /// Letter names, e.g. "kjˈu zˈi" for "qz". Each letter is looked up on
    /// its own: in a phrase, "A" would be read as the article "a".
    fn spell(&self, word: &str) -> Option<String> {
        let names: Vec<String> = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .filter_map(|c| match c.to_ascii_uppercase() {
                'A' => Some("ˈA".to_string()),
                upper => self.known(&upper.to_string()),
            })
            .collect();
        (!names.is_empty()).then(|| names.join(" "))
    }
}

impl Fallback for SegmentingFallback {
    fn phonemize(&self, word: &str) -> Result<String, FallbackError> {
        let lower = word.to_lowercase();
        self.segment(&lower)
            .or_else(|| self.spell(word))
            .ok_or_else(|| FallbackError::NoPhonemes {
                word: word.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_words_are_no_longer_dropped() {
        let g2p = G2P::with_fallback(
            Language::EnglishUS,
            Some(Box::new(SegmentingFallback::new(Language::EnglishUS))),
        );
        for text in ["Zustand is great.", "qzxwv", "Gardner wrote this."] {
            let (phonemes, _) = g2p.g2p(text).unwrap();
            assert!(!phonemes.contains('❓'), "{text} -> {phonemes}");
        }
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    #[test]
    #[ignore]
    fn print_fallbacks() {
        let fb = SegmentingFallback::new(Language::EnglishUS);
        for w in [
            "Kubernetes",
            "Gardner",
            "Tauri",
            "Zustand",
            "hotkeys",
            "dashboarding",
            "qzxwv",
            "Handy",
            "ONNX",
            "misaki",
        ] {
            let t = std::time::Instant::now();
            let r = fb.phonemize(w);
            eprintln!("{w:14} -> {:?} ({:?})", r.ok(), t.elapsed());
        }
    }
}
