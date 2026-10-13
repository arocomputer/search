//! Reading part of a fetched page: query-focused passages, or a window of the
//! text with a continuation offset. Nothing is dropped silently — a reader can
//! always ask for the rest — and no model call is involved.

use std::collections::HashMap;

use crate::core::fetch::Page;

/// One selected passage with the score that chose it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Passage {
    pub text: String,
    pub score: f64,
    /// Character range in the original clean page, with an exclusive end.
    #[serde(default)]
    pub start_offset: usize,
    #[serde(default)]
    pub end_offset: usize,
    /// Resume at this character when the selected paragraph was clipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// What part of a page to return. The default is the whole clean text.
#[derive(Debug, Clone, Default)]
pub struct Focus {
    /// Return the passages that match this query instead of the text.
    pub query: Option<String>,
    /// The most characters to return; absent means no character ceiling.
    /// Machine callers supply the configured read budget unless full is requested.
    pub max_characters: Option<usize>,
    /// The character to start an unfocused read at, from an earlier `next_offset`.
    pub offset: usize,
}

/// A page reduced to what the reader asked for.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusedPage {
    #[serde(flatten)]
    pub page: Page,
    /// Characters in the returned text or passages, excluding separators.
    #[serde(default)]
    pub returned_characters: usize,
    /// Characters in the downloaded clean text before focusing.
    #[serde(default)]
    pub available_characters: usize,
    /// Present when a query matched: the passages to read, in document order,
    /// replacing `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passages: Option<Vec<Passage>>,
    /// Present when more text follows: pass it back as `offset` to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// Apply a [`Focus`] to a fetched page. A query that matches returns passages;
/// one that matches nothing returns the opening of the text instead, so the
/// reader still sees what the page is. A window that ends before the text does
/// sets `next_offset`; `truncated` stays reserved for the fetcher's byte bound.
pub fn focus(mut page: Page, focus: &Focus) -> FocusedPage {
    let available_characters = page.text.chars().count();
    let query = focus.query.as_deref().map(str::trim).unwrap_or_default();
    if !query.is_empty() && !page.text.is_empty() {
        let budget = focus.max_characters.unwrap_or(available_characters);
        let found = select(&page.text, query, budget);
        if found.iter().any(|passage| passage.score > 0.0) {
            page.text = String::new();
            return FocusedPage {
                page,
                returned_characters: found.iter().map(|p| p.text.chars().count()).sum(),
                available_characters,
                passages: Some(found),
                next_offset: None,
            };
        }
    }
    let limit = focus.max_characters.unwrap_or(available_characters);
    let mut rest = page.text.chars().skip(focus.offset);
    let window: String = rest.by_ref().take(limit).collect();
    // A zero-character window reads nothing and must not hand back the same
    // offset, which would spin a caller that follows `next_offset`. A window is
    // not a truncated download, so it never sets `truncated`.
    let next_offset = if limit == 0 {
        None
    } else {
        rest.next().map(|_| focus.offset + window.chars().count())
    };
    page.text = window;
    FocusedPage {
        returned_characters: page.text.chars().count(),
        available_characters,
        page,
        passages: None,
        next_offset,
    }
}

/// Rank the paragraphs of `text` against `query`, returning the best ones up to
/// a character budget including blank-line separators, in document order. With
/// an empty query, or when nothing matches, the opening of the document is
/// returned with score zero.
pub fn select(text: &str, query: &str, budget: usize) -> Vec<Passage> {
    if budget == 0 {
        return Vec::new();
    }
    let paragraphs = split_paragraphs(text);
    if paragraphs.is_empty() {
        return Vec::new();
    }
    let terms = query_terms(query);
    let opening = || {
        vec![Passage {
            text: head(text, budget),
            score: 0.0,
            start_offset: 0,
            end_offset: text.chars().count().min(budget),
            next_offset: (text.chars().count() > budget).then_some(budget),
        }]
    };
    if terms.is_empty() {
        return opening();
    }
    let words: Vec<Vec<String>> = paragraphs.iter().map(|(_, p)| words(p)).collect();

    // Document frequency of each query term across paragraphs, for the IDF
    // weight: a term in every paragraph distinguishes nothing.
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for paragraph in &words {
        for term in &terms {
            if paragraph.iter().any(|word| matches(word, term)) {
                *document_frequency.entry(term.as_str()).or_insert(0) += 1;
            }
        }
    }
    let total = paragraphs.len() as f64;

    let mut scored: Vec<(usize, f64)> = words
        .iter()
        .enumerate()
        .map(|(index, paragraph)| {
            let length = paragraph.len().max(1) as f64;
            let mut score = 0.0;
            for term in &terms {
                let hits = paragraph.iter().filter(|word| matches(word, term)).count();
                if hits == 0 {
                    continue;
                }
                let df = document_frequency.get(term.as_str()).copied().unwrap_or(0) as f64;
                // BM25-style saturation of term frequency, times IDF.
                let idf = ((total - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf = (hits as f64 * 2.2) / (hits as f64 + 1.2 * (0.25 + 0.75 * length / 40.0));
                score += idf * tf;
            }
            (index, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    if scored.is_empty() {
        return opening();
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    // Choose the best paragraphs that fit, then present them in reading order.
    // Joined passages are separated by blank lines, so reserve those two
    // characters per passage; otherwise the joined text exceeds the budget.
    let mut chosen: Vec<(usize, f64, String)> = Vec::new();
    let mut used = 0usize;
    for (index, score) in scored {
        let separator = if chosen.is_empty() { 0 } else { 2 };
        let remaining = budget.saturating_sub(used).saturating_sub(separator);
        if remaining == 0 {
            break;
        }
        let Some((_, paragraph)) = paragraphs.get(index) else {
            continue;
        };
        let selected = head(paragraph, remaining);
        used += separator + selected.chars().count();
        chosen.push((index, score, selected));
    }
    chosen.sort_by_key(|(index, _, _)| *index);
    chosen
        .into_iter()
        .filter_map(|(index, score, text)| {
            let (start_offset, paragraph) = paragraphs.get(index)?;
            let end_offset = start_offset + text.chars().count();
            Some(Passage {
                next_offset: (text.chars().count() < paragraph.chars().count())
                    .then_some(end_offset),
                start_offset: *start_offset,
                end_offset,
                text,
                score,
            })
        })
        .collect()
}

/// Whether a paragraph word matches a query term: exactly, as a prefix for
/// terms of four or more characters (`check` finds `checker`), or inside the
/// word for scripts written without spaces, where a "word" is a whole run.
fn matches(word: &str, term: &str) -> bool {
    word == term
        || (term.chars().count() >= 4 && word.starts_with(term))
        || (term.chars().any(unspaced) && word.contains(term))
}

/// Characters of scripts that do not separate words with spaces.
fn unspaced(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0x0E00..=0x0E7F // Thai
    )
}

/// Lowercased alphanumeric words of a paragraph.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Split on blank lines; fall back to sentences when a page is one long block.
fn split_paragraphs(text: &str) -> Vec<(usize, &str)> {
    let split = |sentences: bool| {
        let parts: Vec<&str> = if sentences {
            text.split_inclusive(['.', '!', '?', '。']).collect()
        } else {
            text.split_inclusive("\n\n").collect()
        };
        let mut offset = 0;
        parts
            .into_iter()
            .filter_map(|part| {
                let start = offset + part.chars().take_while(|c| c.is_whitespace()).count();
                offset += part.chars().count();
                let trimmed = part.trim();
                (!trimmed.is_empty()).then_some((start, trimmed))
            })
            .collect::<Vec<_>>()
    };
    let paragraphs = split(false);
    if paragraphs.len() <= 1 {
        split(true)
    } else {
        paragraphs
    }
}

/// Lowercased query terms, deduplicated. Single characters are kept only in
/// unspaced scripts, where one ideograph can be a word.
fn query_terms(query: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for term in words(query) {
        if (term.chars().count() >= 2 || term.chars().any(unspaced)) && !seen.contains(&term) {
            seen.push(term);
        }
    }
    seen
}

/// The first `budget` characters of `text`, on a character boundary.
fn head(text: &str, budget: usize) -> String {
    text.chars().take(budget).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_long_passage_obeys_the_limit_without_rewriting_punctuation() {
        let passages = select("Keyword! Another sentence?", "keyword", 8);
        assert_eq!(passages[0].text, "Keyword!");
        let passages = select("Keyword followed by lengthy content.", "keyword", 7);
        assert_eq!(passages[0].text, "Keyword");
        assert!(select("Keyword", "keyword", 0).is_empty());
    }

    #[test]
    fn the_matching_paragraph_is_selected_first() {
        let text = "Introduction to the topic.\n\nThe borrow checker enforces memory \
            safety by tracking ownership.\n\nUnrelated closing remarks about licensing.\n\n\
            The borrow checker rejects use after move at compile time.";
        let passages = select(text, "borrow checker ownership", 2000);
        assert!(!passages.is_empty());
        assert!(
            passages[0]
                .text
                .to_ascii_lowercase()
                .contains("borrow checker"),
            "got {:?}",
            passages[0].text
        );
    }

    #[test]
    fn an_empty_query_returns_the_opening() {
        let passages = select("First part.\n\nSecond part.", "", 5000);
        assert_eq!(passages.len(), 1);
        assert!(passages[0].text.starts_with("First part."));
    }

    #[test]
    fn the_budget_is_respected() {
        let text = (0..50)
            .map(|i| format!("paragraph {i} with some filler words and a keyword"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let passages = select(&text, "keyword", 400);
        let total = passages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
            .chars()
            .count();
        assert!(total <= 400, "budget exceeded: {total}");
    }

    #[test]
    fn a_budget_that_cannot_fit_another_separator_stops() {
        let passages = select("keyword\n\nkeyword again", "keyword", 8);
        assert_eq!(passages.len(), 1);
        assert_eq!(passages[0].text, "keyword");
    }

    /// Non-ASCII letters are folded like ASCII ones, so mixed-case accents match.
    #[test]
    fn non_ascii_terms_match_case_insensitively() {
        let text = "Einleitung ohne Treffer.\n\nHier steht ein Absatz über Suchmaschinen.";
        let passages = select(text, "ÜBER", 2000);
        assert_eq!(passages.len(), 1);
        assert!(passages[0].text.contains("über Suchmaschinen"));
    }

    /// Terms match whole words (or word prefixes), never fragments of others.
    #[test]
    fn terms_match_words_not_fragments() {
        let text = "This thesis is long.\n\nThe borrow checker is strict.";
        let passages = select(text, "check", 2000);
        assert_eq!(passages.len(), 1);
        assert!(passages[0].text.contains("checker"));
        let passages = select("Thistle grows here.\n\nNothing else.", "is", 2000);
        assert_eq!(passages[0].score, 0.0, "no false match inside words");
    }

    /// Passages are chosen by score but returned in reading order.
    #[test]
    fn passages_are_returned_in_document_order() {
        let text = "Rust ownership basics.\n\nFiller paragraph.\n\nRust ownership, borrowing, \
            ownership rules and ownership moves.";
        let passages = select(text, "ownership", 2000);
        assert_eq!(passages.len(), 2);
        assert!(passages[0].text.starts_with("Rust ownership basics"));
        assert!(passages[0].score < passages[1].score);
    }

    /// Scripts without spaces match terms inside their runs.
    #[test]
    fn unspaced_scripts_match_inside_runs() {
        let text = "序文です。\n\n私はその人を常に先生と呼んでいた。";
        let passages = select(text, "先生", 2000);
        assert!(passages
            .iter()
            .any(|p| p.score > 0.0 && p.text.contains("先生")));
    }

    /// Explicitly unbounded focused reads must not apply a hidden read default;
    /// machine callers supply their configured limit and download cuts survive.
    #[test]
    fn focused_reads_have_no_hidden_character_default() {
        let text = format!("keyword {}", "é".repeat(7000));
        let mut source = page(&text);
        source.truncated = Some(true);
        let read = focus(
            source,
            &Focus {
                query: Some("keyword".into()),
                ..Default::default()
            },
        );
        assert_eq!(read.returned_characters, text.chars().count());
        assert_eq!(read.page.truncated, Some(true));
        assert_eq!(read.passages.unwrap()[0].next_offset, None);
    }

    /// Offset ranges are original Unicode character positions, including leading
    /// whitespace and separators; continuation resumes a clipped paragraph.
    #[test]
    fn passages_expose_original_unicode_offsets_and_continuations() {
        let text = "序文。\n\n  keyword café and details  ";
        let selected = select(text, "keyword", 10);
        let passage = &selected[0];
        assert_eq!(passage.start_offset, 7);
        assert_eq!(passage.end_offset, 17);
        assert_eq!(passage.next_offset, Some(17));
        assert_eq!(
            text.chars()
                .skip(passage.start_offset)
                .take(passage.end_offset - passage.start_offset)
                .collect::<String>(),
            passage.text
        );
        let read = focus(
            page(text),
            &Focus {
                query: Some("keyword".into()),
                max_characters: Some(10),
                ..Default::default()
            },
        );
        assert_eq!(read.returned_characters, 10);
        assert_eq!(read.available_characters, text.chars().count());
        assert_eq!(read.passages.unwrap()[0].next_offset, Some(17));
    }

    fn page(text: &str) -> Page {
        Page {
            url: "https://example.com".into(),
            fetched_at: None,
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            title: None,
            byline: None,
            published: None,
            site: None,
            text: text.into(),
            links: Vec::new(),
            truncated: None,
            redirect: None,
            error: None,
        }
    }

    /// A window that stops early says where to continue, and continuing
    /// reaches the end without losing or repeating a character.
    #[test]
    fn windows_continue_from_next_offset() {
        let first = focus(
            page("abcdefgh"),
            &Focus {
                max_characters: Some(3),
                ..Focus::default()
            },
        );
        assert_eq!(first.page.text, "abc");
        // A window is not a truncated download: only the fetcher's byte bound
        // sets `truncated`. Continuation is signalled by `next_offset` alone.
        assert_eq!(first.page.truncated, None);
        assert_eq!(first.next_offset, Some(3));
        let last = focus(
            page("abcdefgh"),
            &Focus {
                max_characters: Some(5),
                offset: 3,
                ..Focus::default()
            },
        );
        assert_eq!(last.page.text, "defgh");
        assert_eq!(last.next_offset, None);
        assert_eq!(last.page.truncated, None);
        let whole = focus(page("abcdefgh"), &Focus::default());
        assert_eq!(
            (whole.page.text.as_str(), whole.next_offset),
            ("abcdefgh", None)
        );
    }

    /// A zero-character window reads nothing and continues nowhere, so a caller
    /// that follows `next_offset` cannot spin.
    #[test]
    fn a_zero_window_does_not_continue() {
        let read = focus(
            page("abcdef"),
            &Focus {
                max_characters: Some(0),
                ..Focus::default()
            },
        );
        assert_eq!(read.page.text, "");
        assert_eq!(read.next_offset, None);
    }

    /// A query that matches returns passages; one that does not returns the
    /// opening of the text with a continuation, never a fake match.
    #[test]
    fn focused_reads_fall_back_to_the_opening() {
        let matched = focus(
            page("Intro.\n\nThe borrow checker."),
            &Focus {
                query: Some("borrow".into()),
                ..Focus::default()
            },
        );
        assert!(matched.page.text.is_empty());
        assert_eq!(matched.passages.unwrap()[0].text, "The borrow checker.");
        let unmatched = focus(
            page("Intro text."),
            &Focus {
                query: Some("absent".into()),
                max_characters: Some(5),
                ..Focus::default()
            },
        );
        assert!(unmatched.passages.is_none());
        assert_eq!(unmatched.page.text, "Intro");
        assert_eq!(unmatched.next_offset, Some(5));
    }
}
