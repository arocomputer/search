//! Render responses as control-free terminal text or unchanged machine-readable JSON.

use crate::core::engines::EngineStatus;
use crate::core::text::FocusedPage;
use crate::core::Answer;

/// Remove terminal and Unicode direction controls from human text, retaining
/// newlines and tabs for page layout. Never apply this to machine-readable data.
pub(super) fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|&c| {
            (c == '\n' || c == '\t' || !c.is_control())
                && !matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// Print any serializable response as JSON; return a process-style status.
pub fn json<T: serde::Serialize>(value: &T) -> i32 {
    match serde_json::to_string_pretty(value) {
        Ok(output) => {
            println!("{output}");
            0
        }
        Err(message) => error(message.to_string()),
    }
}

/// Render web results and engine health for a person at a terminal: failed,
/// timed-out, and row-skipping engines are reported on stderr.
pub fn search(answer: &Answer) -> i32 {
    for (rank, link) in answer.results.iter().enumerate() {
        println!(
            "{}. {}\n   {}",
            rank + 1,
            terminal_text(&link.title),
            terminal_text(&link.url)
        );
        if let Some(snippet) = &link.snippet {
            println!("   {}", terminal_text(snippet));
        }
        println!();
    }
    eprintln!(
        "{} results · {} ms",
        answer.results.len(),
        answer.duration_ms
    );
    engine_health(&answer.engines);
    0
}

/// Report partial engine execution and unsupported filters without hiding empty answers.
fn engine_health(engines: &[crate::core::engines::EngineState]) {
    for engine in engines {
        if !engine.unsupported_filters.is_empty() {
            eprintln!(
                "{}: unsupported filters: {}",
                terminal_text(&engine.name),
                terminal_text(&engine.unsupported_filters.join(", "))
            );
        }
        let status = match engine.status {
            EngineStatus::Error => "failed",
            EngineStatus::Timeout => "timed out",
            EngineStatus::Ok if engine.skipped > 0 => "ok",
            EngineStatus::Ok => continue,
        };
        let skipped = match engine.skipped {
            0 => String::new(),
            count => format!(", skipped {count} invalid results"),
        };
        eprintln!(
            "{}: {status}{skipped}{}",
            terminal_text(&engine.name),
            engine
                .error
                .as_ref()
                .map(|error| format!(": {}", terminal_text(error)))
                .unwrap_or_default()
        );
    }
}

/// Render fetched page text. A failed URL is printed in place, so its result stays
/// with its position; the caller decides the exit status.
pub fn fetch(results: &[FocusedPage]) {
    for read in results {
        let page = &read.page;
        println!(
            "## {}",
            terminal_text(page.title.as_deref().unwrap_or(&page.url))
        );
        println!(
            "{}\n",
            terminal_text(page.final_url.as_deref().unwrap_or(&page.url))
        );
        if let Some(error) = &page.error {
            println!("Fetch failed: {}\n", terminal_text(error));
            continue;
        }
        match &read.passages {
            Some(passages) => {
                for passage in passages {
                    println!("{}\n", terminal_text(&passage.text));
                    if let Some(offset) = passage.next_offset {
                        println!(
                            "[passage continues; read on without -query using -offset {offset}]\n"
                        );
                    }
                }
            }
            None => println!("{}\n", terminal_text(&page.text)),
        }
        if let Some(offset) = read.next_offset {
            println!("[continues; read on with -offset {offset}]\n");
        }
        if page.truncated == Some(true) {
            println!("[truncated at fetch.max_response_bytes]\n");
        }
    }
}

/// Render an evidence packet for a person: each source with its date and
/// passages, and the coverage line on stderr.
pub fn research(packet: &crate::core::Packet) -> i32 {
    if let Some(synthesis) = &packet.synthesis {
        println!("{}\n", terminal_text(synthesis));
    }
    for error in &packet.processing_errors {
        eprintln!("processing: {}", terminal_text(error));
    }
    for source in &packet.sources {
        let title = source.title.as_deref().unwrap_or(&source.url);
        println!("## {}", terminal_text(title));
        println!(
            "{}\n({})\n",
            terminal_text(&source.url),
            terminal_text(source.published.as_deref().unwrap_or("no date"))
        );
        if let Some(error) = &source.error {
            println!("Fetch failed: {}\n", terminal_text(error));
            continue;
        }
        for passage in &source.passages {
            println!("{}\n", terminal_text(&passage.text));
            if let Some(offset) = passage.next_offset {
                println!("[passage continues; search read URL -offset {offset}]\n");
            }
        }
        if let Some(offset) = source.next_offset {
            println!("[continues; search read URL -offset {offset}]\n");
        }
        if source.truncated {
            println!("[truncated at fetch.max_response_bytes]\n");
        }
    }
    engine_health(&packet.engines);
    if packet.timed_out {
        eprintln!("research: timed out");
    }
    if packet.incomplete {
        eprintln!("research: incomplete evidence packet");
    }
    eprintln!(
        "{} sources · {} candidates · {} read · {} failed · {} skipped · {} chars · {} ms",
        packet.sources.len(),
        packet.coverage.candidates,
        packet.coverage.read,
        packet.coverage.failed,
        packet.coverage.skipped,
        packet.characters,
        packet.duration_ms,
    );
    0
}

/// Print a human diagnostic without terminal controls and return failure status.
pub fn error(message: String) -> i32 {
    eprintln!("search: {}", terminal_text(&message));
    1
}

/// Render a site inventory with honest bounds and per-URL failures.
pub fn crawl(report: &crate::core::CrawlReport) -> i32 {
    for url in &report.urls {
        println!("{}", terminal_text(url));
    }
    for page in &report.pages {
        println!(
            "\n## {}\n{}\n\n{}",
            terminal_text(page.title.as_deref().unwrap_or(&page.url)),
            terminal_text(&page.url),
            terminal_text(&page.text)
        );
        if page.truncated == Some(true) {
            eprintln!(
                "{}: truncated at fetch.max_response_bytes",
                terminal_text(&page.url)
            );
        }
        if let Some(window) = report.windows.iter().find(|window| window.url == page.url) {
            if let Some(offset) = window.next_offset {
                eprintln!(
                    "{}: continue with search read URL -offset {offset}",
                    terminal_text(&page.url)
                );
            }
        }
    }
    eprintln!(
        "{} visited · {} bytes · {} ms{}{}",
        report.visited,
        report.bytes,
        report.duration_ms,
        if report.timed_out {
            " · timed out"
        } else {
            ""
        },
        if report.truncated { " · bounded" } else { "" }
    );
    for failure in &report.failures {
        eprintln!(
            "{}",
            terminal_text(&serde_json::to_string(failure).unwrap_or_default())
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OSC52, CSI, C1 equivalents, and cursor controls lose their control bytes.
    #[test]
    fn terminal_text_neutralizes_terminal_actions() {
        let attack = "title\x1b]52;c;Y2xpcA==\x07\x1b[2J\x1b[H\r\x08\x00\x7f\u{009b}31m\u{009d}52;c;data\u{009c}";
        let rendered = terminal_text(attack);
        assert_eq!(rendered, "title]52;c;Y2xpcA==[2J[H31m52;c;data");
        assert!(!rendered.chars().any(char::is_control));
    }

    /// Unicode direction overrides are removed without damaging readable Unicode or layout.
    #[test]
    fn terminal_text_preserves_text_layout_without_direction_controls() {
        let text = "café 日本語 👩\u{200d}💻\n\tparagraph";
        assert_eq!(terminal_text(text), text);
        assert_eq!(
            terminal_text("a\u{061c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}b"),
            "ab"
        );
    }
}
