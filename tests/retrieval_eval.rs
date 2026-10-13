//! Small, labeled offline retrieval contracts, not a benchmark or live-search
//! quality claim. Command discovery and loopback HTTP exercise the real core.

use std::collections::{BTreeMap, BTreeSet};

use search::core::research::{Packet, ResearchOptions};
use search::core::{Config, Search};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// Short, verbatim NPS excerpts, retrieved 2026-10-13. Labels identify complete
// answer-bearing sentences, rather than rewarding query-word overlap.
// https://www.nps.gov/yell/learn/nature/ecological-consequences-of-fire.htm
const CONES: &str = "Serotinous cones will not release their seeds until the resin sealing them melts, requiring a temperature of at least 113°F (45°C).";
// https://www.nps.gov/glac/learn/nature/fire-ecology.htm
const NUTRIENTS: &str = "Downed logs and duff on the forest floor are burned to ash, releasing nutrients back into the soil.";
// https://www.nps.gov/articles/wildland-fire-lodgepole-pine.htm
const BARK: &str =
    "The bark of lodgepoles is thin, which does not protect the trunks from scorching by fire.";
// https://www.nps.gov/romo/learn/fire-ecology.htm (contact information).
const DISTRACTOR: &str = "The Information Office is open year-round. During summer, the office is open from 8 am to 4 pm daily.";

const TREE_QUERY: &str = "serotinous cones resin bark scorching";
const SOIL_QUERY: &str = "nutrients soil";

/// Own a loopback-only server; dropping it aborts the accept loop even on failure.
struct Pages {
    port: u16,
    task: JoinHandle<()>,
}

impl Pages {
    /// Serve real excerpts inside constructed HTML articles, with an unrelated
    /// opening so returning the beginning cannot satisfy the passage labels.
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let excerpt = match path {
                    "/cones" | "/mirror-a" | "/mirror-b" => Some(CONES),
                    "/nutrients" => Some(NUTRIENTS),
                    "/bark" => Some(BARK),
                    _ => None,
                };
                let (status, body) = match excerpt {
                    Some(text) => (
                        "200 OK",
                        format!("<!doctype html><html><head><title>Field notes</title></head><body><article><p>{DISTRACTOR}</p><p>{text}</p></article></body></html>"),
                    ),
                    None => ("404 Not Found", "missing page".to_owned()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self { port, task }
    }

    /// Numeric loopback and localhost are distinct host identities, regardless
    /// of port. Neither fixture hostname reaches the public network.
    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Configure one command engine with query-specific ranked answers in settings.
/// Python is the same external fixture runtime used by the engine contracts;
/// it performs no network IO and needs no third-party modules or script files.
fn search(answers: &[(&str, Vec<String>)]) -> Search {
    let answers: BTreeMap<_, _> = answers
        .iter()
        .map(|(query, urls)| {
            let rows: Vec<Value> = urls
                .iter()
                .map(|url| json!({"title": "Field notes", "url": url}))
                .collect();
            (*query, rows)
        })
        .collect();
    let config: Config = serde_json::from_value(json!({
        "search": {"max_results": 20, "engine_timeout": 5000},
        "fetch": {"allow_private_networks": true, "timeout": 5000},
        "research": {"max_per_host": 3},
        "engines": {
            "use": ["offline-eval"],
            "config": {"offline-eval": {
                "type": "command",
                "command": if cfg!(windows) { "python.exe" } else { "python3" },
                "version": 2,
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "args": ["-S", "-B", "-c", "import json,sys; r=json.load(sys.stdin); print(json.dumps({'results':r['config']['answers'][r['query']][:r['limit']]}))"],
                "config": {"answers": answers}
            }}
        }
    }))
    .unwrap();
    Search::open(config).unwrap()
}

/// Explicit ceilings allow three complete labeled sentences but exclude the
/// unrelated opening paragraphs. Discovery has room for replacement attempts.
fn options(sources: usize) -> ResearchOptions {
    ResearchOptions {
        sources,
        source_characters: 160,
        total_characters: 400,
        max_per_host: 3,
        max_attempts: 12,
        ..ResearchOptions::default()
    }
}

/// Source recall counts only labeled URLs that actually returned passages;
/// discovery hits and failed-source provenance receive no credit.
fn source_recall(packet: &Packet, labels: &[String]) -> f64 {
    let hits = labels
        .iter()
        .filter(|url| {
            packet.sources.iter().any(|source| {
                source.url == **url && source.error.is_none() && !source.passages.is_empty()
            })
        })
        .count();
    hits as f64 / labels.len() as f64
}

/// Return passage text alone: titles and upstream snippets cannot earn labels.
fn evidence_text(packet: &Packet) -> String {
    packet
        .sources
        .iter()
        .flat_map(|source| &source.passages)
        .map(|passage| passage.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Complementary discovery must increase useful-source recall from 2/3 to 3/3.
#[tokio::test]
async fn complementary_queries_recover_all_labeled_sources() {
    let pages = Pages::new().await;
    let cones = pages.url("127.0.0.1", "/cones");
    let bark = pages.url("127.0.0.1", "/bark");
    let nutrients = pages.url("127.0.0.1", "/nutrients");
    let labels = [cones.clone(), bark.clone(), nutrients.clone()];
    let search = search(&[
        (TREE_QUERY, vec![cones, bark]),
        (SOIL_QUERY, vec![nutrients]),
    ]);
    let single = search.research(vec![TREE_QUERY.into()], options(3)).await;
    let combined = search
        .research(vec![TREE_QUERY.into(), SOIL_QUERY.into()], options(3))
        .await;
    assert_eq!(source_recall(&single, &labels), 2.0 / 3.0, "{single:#?}");
    assert_eq!(source_recall(&combined, &labels), 1.0, "{combined:#?}");
    let text = evidence_text(&combined);
    for fact in [CONES, BARK, NUTRIENTS] {
        assert!(
            text.contains(fact),
            "missing complete fact {fact:?}: {text}"
        );
    }
}

/// Pin fact recall and labeled-character precision together: an empty or
/// clipped packet cannot pass by merely respecting its character ceiling.
#[tokio::test]
async fn character_budget_keeps_facts_instead_of_unrelated_openings() {
    let pages = Pages::new().await;
    let search = search(&[
        (
            TREE_QUERY,
            vec![
                pages.url("127.0.0.1", "/cones"),
                pages.url("127.0.0.1", "/bark"),
            ],
        ),
        (SOIL_QUERY, vec![pages.url("127.0.0.1", "/nutrients")]),
    ]);
    let packet = search
        .research(vec![TREE_QUERY.into(), SOIL_QUERY.into()], options(3))
        .await;
    let text = evidence_text(&packet);
    let facts = [CONES, BARK, NUTRIENTS];
    let recovered = facts.iter().filter(|fact| text.contains(**fact)).count();
    assert_eq!(recovered, facts.len(), "fact recall: {packet:#?}");
    let returned: usize = packet
        .sources
        .iter()
        .flat_map(|source| &source.passages)
        .map(|passage| passage.text.chars().count())
        .sum();
    assert_eq!(packet.characters, returned);
    assert!(returned <= 400, "explicit total budget: {returned}");
    for source in &packet.sources {
        let count: usize = source.passages.iter().map(|p| p.text.chars().count()).sum();
        assert!(count <= 160, "explicit source budget: {source:#?}");
    }
    let labeled: usize = facts.iter().map(|fact| fact.chars().count()).sum();
    let precision = labeled as f64 / returned as f64;
    assert!(
        precision >= 0.85,
        "labeled-character precision: {precision}"
    );
    let available: usize = packet.sources.iter().map(|s| s.available_characters).sum();
    let fraction_read = returned as f64 / available as f64;
    assert!(
        fraction_read <= 0.65,
        "fraction of clean text read: {fraction_read}"
    );
}

/// Two content mirrors and a leading 404 must not consume useful-source slots
/// or budget, and the report must acknowledge all three wasted attempts.
#[tokio::test]
async fn duplicates_and_dead_links_do_not_evict_useful_sources() {
    let pages = Pages::new().await;
    let labels = ["/cones", "/nutrients", "/bark"].map(|path| pages.url("127.0.0.1", path));
    let query = format!("{TREE_QUERY} {SOIL_QUERY}");
    let urls = [
        "/dead",
        "/cones",
        "/mirror-a",
        "/mirror-b",
        "/nutrients",
        "/bark",
    ]
    .map(|path| pages.url("127.0.0.1", path));
    let search = search(&[(&query, urls.into())]);
    let packet = search.research(vec![query], options(3)).await;
    assert_eq!(source_recall(&packet, &labels), 1.0, "{packet:#?}");
    assert_eq!(packet.coverage.read, 3, "{packet:#?}");
    assert_eq!(packet.coverage.failed, 1, "{packet:#?}");
    assert_eq!(packet.coverage.duplicate, 2, "{packet:#?}");
    assert_eq!(packet.coverage.attempted, 6, "{packet:#?}");
    let text = evidence_text(&packet);
    assert_eq!(text.matches(CONES).count(), 1, "mirror content used budget");
    assert!(text.contains(NUTRIENTS) && text.contains(BARK), "{text}");
}

/// A host quota must recover the independent soil source behind a crowded
/// host. Different ports on the same hostname must still share the quota.
#[tokio::test]
async fn host_diversity_preserves_complementary_evidence() {
    let first = Pages::new().await;
    let second = Pages::new().await;
    let cones = first.url("127.0.0.1", "/cones");
    let nutrients = second.url("localhost", "/nutrients");
    let labels = [cones.clone(), nutrients.clone()];
    let query = format!("{TREE_QUERY} {SOIL_QUERY}");
    let search = search(&[(
        &query,
        vec![
            cones,
            second.url("127.0.0.1", "/bark"),
            first.url("127.0.0.1", "/nutrients"),
            nutrients,
        ],
    )]);
    let crowded = search.research(vec![query.clone()], options(2)).await;
    let mut diverse_options = options(2);
    diverse_options.max_per_host = 1;
    let diverse = search.research(vec![query], diverse_options).await;
    assert_eq!(source_recall(&crowded, &labels), 0.5, "{crowded:#?}");
    assert_eq!(source_recall(&diverse, &labels), 1.0, "{diverse:#?}");
    let hosts: BTreeSet<_> = diverse
        .sources
        .iter()
        .filter(|s| !s.passages.is_empty())
        .map(|s| {
            url::Url::parse(s.final_url.as_deref().unwrap_or(&s.url))
                .unwrap()
                .host_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(hosts.len(), 2, "{diverse:#?}");
    assert_eq!(
        diverse.coverage.skipped, 2,
        "same-host candidates across ports"
    );
    assert!(evidence_text(&diverse).contains(NUTRIENTS));
}
