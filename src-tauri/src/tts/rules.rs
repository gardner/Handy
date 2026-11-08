//! Last-resort English letter-to-sound rules for words no dictionary knows
//! ("Kubernetes", "Zustand"). Far from perfect, but a guessed pronunciation
//! is much easier to follow than the word spelled out letter by letter.
//!
//! Approach: match the longest known spelling pattern at each position;
//! single vowels are long in an open syllable (one consonant then a vowel:
//! "ta-ble") and short in a closed one ("tab-let"). Primary stress goes on the
//! first vowel, the common case for English names.

/// Pronounce a lowercase ASCII word, or `None` if it has no vowel to anchor a
/// pronunciation (those are better spelled out).
pub fn pronounce(word: &str) -> Option<String> {
    let w: Vec<char> = word.chars().collect();
    if w.len() < 3 || !w.iter().all(|c| c.is_ascii_lowercase()) || !w.iter().any(|c| is_vowel(*c)) {
        return None;
    }

    let mut out = String::new();
    let mut stressed = false;
    let mut i = 0;
    while i < w.len() {
        let rest: String = w[i..].iter().collect();
        let at_start = i == 0;
        let at_end = |len: usize| i + len == w.len();

        // Vowel groups and r-colored vowels.
        let vowel = [
            ("eau", "O"),
            ("ee", "i"),
            ("ea", "i"),
            ("ie", "i"),
            ("ei", "A"),
            ("ey", "A"),
            ("ai", "A"),
            ("ay", "A"),
            ("au", "ɔ"),
            ("aw", "ɔ"),
            ("oo", "u"),
            ("ou", "W"),
            ("ow", "O"),
            ("oi", "Y"),
            ("oy", "Y"),
            ("oa", "O"),
            ("ue", "u"),
            ("ew", "u"),
            ("ar", "ɑɹ"),
            ("or", "ɔɹ"),
            ("er", "ɜɹ"),
            ("ir", "ɜɹ"),
            ("ur", "ɜɹ"),
        ]
        .iter()
        .find(|(pat, _)| {
            rest.starts_with(pat) && !(pat.ends_with('r') && next_is_vowel(&w, i + 2))
        });
        if let Some((pat, ph)) = vowel {
            push_vowel(&mut out, ph, &mut stressed);
            i += pat.len();
            continue;
        }

        let c = w[i];
        if is_vowel(c) || (c == 'y' && !at_start) {
            // Silent final e ("Rune", "Tate"), but not in short words ("the").
            if c == 'e' && at_end(1) && w.len() > 3 && !is_vowel(w[i - 1]) {
                i += 1;
                continue;
            }
            let ph = if at_end(1) {
                match c {
                    'a' => "ə",
                    'e' | 'i' | 'y' => "i",
                    'o' => "O",
                    _ => "u",
                }
            } else if open_syllable(&w, i) {
                match c {
                    'a' => "A",
                    'e' => "i",
                    'i' | 'y' => "I",
                    'o' => "O",
                    _ => "u",
                }
            } else {
                match c {
                    'a' => "æ",
                    'e' => "ɛ",
                    'i' | 'y' => "ɪ",
                    'o' => "ɑ",
                    _ => "ʌ",
                }
            };
            push_vowel(&mut out, ph, &mut stressed);
            i += 1;
            continue;
        }

        // Consonant patterns, longest first.
        let consonants = [
            ("tch", "ʧ"),
            ("dge", "ʤ"),
            ("sch", "sk"),
            ("ch", "ʧ"),
            ("sh", "ʃ"),
            ("th", "θ"),
            ("ph", "f"),
            ("wh", "w"),
            ("ck", "k"),
            ("ng", "ŋ"),
            ("qu", "kw"),
            ("gh", if at_start { "ɡ" } else { "" }),
            ("kn", if at_start { "n" } else { "kn" }),
            ("wr", "ɹ"),
        ];
        let consonant = consonants.iter().find(|(pat, _)| rest.starts_with(pat));
        if let Some((pat, ph)) = consonant {
            out.push_str(ph);
            i += pat.len();
            continue;
        }

        // Doubled consonants sound once ("Gardner", "Hubble").
        if i + 1 < w.len() && w[i + 1] == c {
            i += 1;
            continue;
        }
        let soft = w.get(i + 1).is_some_and(|n| matches!(n, 'e' | 'i' | 'y'));
        out.push_str(match c {
            'c' if soft => "s",
            'c' => "k",
            'g' => "ɡ",
            'j' => "ʤ",
            'q' => "k",
            'r' => "ɹ",
            'x' if at_start => "z",
            'x' => "ks",
            'y' => "j",
            other => match other {
                'b' => "b",
                'd' => "d",
                'f' => "f",
                'h' => "h",
                'k' => "k",
                'l' => "l",
                'm' => "m",
                'n' => "n",
                'p' => "p",
                's' => "s",
                't' => "t",
                'v' => "v",
                'w' => "w",
                'z' => "z",
                _ => "",
            },
        });
        i += 1;
    }
    (!out.is_empty()).then_some(out)
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

fn next_is_vowel(w: &[char], i: usize) -> bool {
    w.get(i).is_some_and(|c| is_vowel(*c))
}

/// One consonant and then a vowel follow: the vowel says its name.
fn open_syllable(w: &[char], i: usize) -> bool {
    matches!(
        (w.get(i + 1), w.get(i + 2)),
        (Some(c), Some(v)) if !is_vowel(*c) && (is_vowel(*v) || *v == 'y')
    )
}

fn push_vowel(out: &mut String, phoneme: &str, stressed: &mut bool) {
    if !*stressed {
        out.push('ˈ');
        *stressed = true;
    }
    out.push_str(phoneme);
}

#[cfg(test)]
mod tests {
    use super::pronounce;

    #[test]
    fn guesses_unknown_words() {
        assert_eq!(pronounce("zustand").as_deref(), Some("zˈʌstænd"));
        assert_eq!(pronounce("tauri").as_deref(), Some("tˈɔɹi"));
        assert_eq!(pronounce("kubernetes").as_deref(), Some("kˈubɜɹnitɛs"));
    }

    #[test]
    fn leaves_unpronounceable_words_to_spelling() {
        assert_eq!(pronounce("qzxwv"), None);
        assert_eq!(pronounce("ab"), None);
    }
}
