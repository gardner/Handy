//! Turns raw selected text into chunks that read well aloud.
//!
//! Selections come from web pages, PDFs, chat apps and code editors, so they
//! carry things that sound bad when spoken literally: URLs, citation marks,
//! markdown symbols, and line breaks in the middle of sentences.

use once_cell::sync::Lazy;
use regex::Regex;

static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(?:https?://|www\.)\S+").unwrap());
static CITATION: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\d+(?:[,\u{2013}-]\s*\d+)*\]").unwrap());
// A markdown heading is its own paragraph.
static HEADING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s{0,3}#{1,6}\s+(.*)$").unwrap());
// Markdown emphasis, block quotes, inline code.
static MARKDOWN: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s{0,3}>+\s+|[*_`~]{1,3}").unwrap());
// A bullet or number at the start of a line becomes a sentence break.
static LIST_ITEM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:[-•*▪◦]|\d{1,3}[.)])\s+").unwrap());
// "exam-\nple" -> "example" (word hyphenated across a line break).
static HYPHEN_BREAK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(\p{L})-\n\s*(\p{Ll})").unwrap());
static BLANK_LINES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n\s*\n+").unwrap());
static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// Clean `text` for speech. Paragraph breaks and list items become sentence
/// breaks so the voice pauses there; other line breaks are joined.
pub fn clean(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = HYPHEN_BREAK.replace_all(&text, "$1$2");
    let text = URL.replace_all(&text, "link");
    let text = CITATION.replace_all(&text, "");
    let text = LIST_ITEM.replace_all(&text, "\n\n");
    let text = HEADING.replace_all(&text, "\n\n$1\n\n");
    let text = MARKDOWN.replace_all(&text, "");
    // Each paragraph ends in punctuation so the voice pauses between them.
    BLANK_LINES
        .split(&text)
        .map(|p| SPACES.replace_all(p, " ").trim().to_string())
        .filter(|p| p.chars().any(char::is_alphanumeric))
        .map(|mut p| {
            if !p.ends_with(|c: char| ".!?…;:,".contains(c)) {
                p.push('.');
            }
            p
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split cleaned text into sentences, the unit Handy synthesizes and plays.
/// Short pieces are merged into the next sentence so the voice doesn't pause
/// unnaturally after abbreviations or one-word fragments.
pub fn sentences(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        current.push(c);
        let is_end = matches!(c, '.' | '!' | '?' | '…' | ';')
            && chars
                .get(i + 1)
                .is_none_or(|n| n.is_whitespace() || *n == '"' || *n == '”');
        if is_end {
            // Swallow closing quotes and repeated punctuation ("?!", "...").
            while let Some(&n) = chars.get(i + 1) {
                if matches!(n, '.' | '!' | '?' | '"' | '”' | ')') {
                    current.push(n);
                    i += 1;
                } else {
                    break;
                }
            }
            push_sentence(&mut out, &mut current);
        }
        i += 1;
    }
    push_sentence(&mut out, &mut current);
    out
}

fn push_sentence(out: &mut Vec<String>, current: &mut String) {
    let s = current.trim();
    if s.chars().any(char::is_alphanumeric) {
        // Merge fragments too short to stand alone (e.g. "Dr." or "Wow!").
        if let Some(last) = out.last_mut() {
            if last.split_whitespace().count() < 3 {
                last.push(' ');
                last.push_str(s);
                current.clear();
                return;
            }
        }
        out.push(s.to_string());
    } else if let Some(last) = out.last_mut() {
        // Stray punctuation belongs to the previous sentence.
        last.push_str(s);
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_soft_line_breaks_and_hyphenation() {
        assert_eq!(
            clean("This is an exam-\nple of a sentence\nthat wraps."),
            "This is an example of a sentence that wraps."
        );
    }

    #[test]
    fn paragraphs_become_sentence_breaks() {
        assert_eq!(clean("Heading\n\nBody text."), "Heading. Body text.");
    }

    #[test]
    fn strips_urls_citations_and_markdown() {
        assert_eq!(
            clean("## Title\nSee **this** [12] at https://example.com/a?b=c now."),
            "Title. See this at link now."
        );
    }

    #[test]
    fn list_items_pause() {
        assert_eq!(
            clean("Shopping:\n- eggs\n- milk\n1. bread"),
            "Shopping: eggs. milk. bread."
        );
    }

    #[test]
    fn splits_sentences() {
        assert_eq!(
            sentences("Hello there, friend. How are you today? I am fine!"),
            vec!["Hello there, friend.", "How are you today?", "I am fine!"]
        );
    }

    #[test]
    fn keeps_decimals_and_merges_short_fragments() {
        assert_eq!(
            sentences("It costs 3.50 dollars. Wow! That is a lot of money."),
            vec!["It costs 3.50 dollars.", "Wow! That is a lot of money."]
        );
    }

    #[test]
    fn handles_text_without_final_punctuation() {
        assert_eq!(
            sentences("no punctuation here"),
            vec!["no punctuation here"]
        );
        assert_eq!(clean("no punctuation here"), "no punctuation here.");
    }

    #[test]
    fn empty_input_gives_nothing() {
        assert!(sentences(&clean("  \n\n ")).is_empty());
    }
}
