//! Explicit, bounded edge processing of an already auditable evidence packet.
//! Trusted operator programs receive JSON, never interpolated prompt arguments.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::ResearchRequest;
use crate::core::config::{Adapter, Command};
use crate::core::{Packet, Search};

/// Run only the requested configured processors, preserving evidence on a
/// processor failure. Remote callers execute processing on the paired host.
pub(crate) async fn process(
    search: &Search,
    request: &ResearchRequest,
    mut packet: Packet,
) -> Result<Packet, String> {
    let settings = &search.config().processing;
    let started = std::time::Instant::now();
    let research_timeout = request
        .options
        .clone()
        .configured(&search.config().research)
        .timeout;
    let remaining = research_timeout.saturating_sub(packet.duration_ms);
    let budget = Duration::from_millis(settings.timeout.min(remaining));
    let deadline = tokio::time::Instant::now() + budget;
    for (enabled, operation, command) in [
        (request.rerank, "rerank", settings.reranker.as_ref()),
        (
            request.synthesize,
            "synthesize",
            settings.synthesizer.as_ref(),
        ),
    ] {
        if !enabled {
            continue;
        }
        let Some(command) = command else {
            return Err(format!("{operation} is not configured on this host"));
        };
        let result = if packet.timed_out || tokio::time::Instant::now() >= deadline {
            Err(format!("{operation} timed out"))
        } else {
            tokio::time::timeout_at(deadline, async {
                let _permit = search
                    .processing_permits
                    .acquire()
                    .await
                    .map_err(|_| "processor admission unavailable".to_string())?;
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!("{operation} timed out"));
                }
                exchange(command, operation, &packet, settings.max_output_bytes).await
            })
            .await
            .map_err(|_| format!("{operation} timed out"))
            .and_then(|result| result)
            .and_then(|value| apply(operation, value, &mut packet))
        };
        if let Err(error) = result {
            packet.processing_errors.push(error);
        }
    }
    packet.duration_ms = packet
        .duration_ms
        .saturating_add(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    if (request.rerank || request.synthesize) && packet.duration_ms >= research_timeout {
        packet.timed_out = true;
        packet.incomplete = true;
    }
    Ok(packet)
}

/// Exchange one bounded JSON document with a cancellation-safe direct child.
async fn exchange(
    settings: &Command,
    operation: &str,
    packet: &Packet,
    cap: u64,
) -> Result<serde_json::Value, String> {
    Adapter::Command(settings.clone())
        .validate()
        .map_err(|_| "processor configuration is invalid")?;
    crate::core::engines::prepare_command_scratch(&settings.temp_dir)
        .map_err(|_| "processor temporary directory unavailable or unsafe")?;
    // Match command engines: absolute programs default to their containing
    // directory; bare PATH programs use private scratch, never the caller cwd.
    let program = std::path::Path::new(&settings.command);
    let cwd = settings.cwd.as_deref().unwrap_or_else(|| {
        if program.is_absolute() {
            program.parent().unwrap_or(&settings.temp_dir)
        } else {
            &settings.temp_dir
        }
    });
    let mut command = tokio::process::Command::new(&settings.command);
    command.args(&settings.args).current_dir(cwd).env_clear();
    for name in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "PATHEXT",
        "LANG",
        "SystemRoot",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for name in &settings.env {
        if matches!(name.as_str(), "TMPDIR" | "TMP" | "TEMP") {
            continue;
        }
        let value = std::env::var_os(name).ok_or("processor credential unavailable")?;
        command.env(name, value);
    }
    for name in ["TMPDIR", "TMP", "TEMP"] {
        command.env(name, &settings.temp_dir);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "processor failed to start")?;
    let mut stdin = child.stdin.take().ok_or("processor input unavailable")?;
    let stdout = child.stdout.take().ok_or("processor output unavailable")?;
    let request = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "operation": operation,
        "packet": packet,
        "config": settings.config,
    }))
    .map_err(|_| "processor request encoding failed")?;
    let cap = cap.min(settings.max_response_bytes);
    let write = async {
        stdin
            .write_all(&request)
            .await
            .map_err(|_| "processor write failed")?;
        stdin
            .shutdown()
            .await
            .map_err(|_| "processor write failed")?;
        drop(stdin);
        Ok::<(), String>(())
    };
    let read = async {
        let mut bytes = Vec::new();
        stdout
            .take(cap.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "processor read failed")?;
        if bytes.len() as u64 > cap {
            return Err("processor output exceeded cap".to_string());
        }
        Ok(bytes)
    };
    let (_, bytes) = tokio::try_join!(write, read)?;
    if !child
        .wait()
        .await
        .map_err(|_| "processor wait failed")?
        .success()
    {
        return Err("processor exited unsuccessfully".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "processor returned invalid JSON".into())
}

/// Accept only a permutation of evidence URLs or a cited answer. Programs may
/// not fabricate sources, replace passages, or suppress the coverage report.
fn apply(operation: &str, value: serde_json::Value, packet: &mut Packet) -> Result<(), String> {
    let valid: HashSet<&str> = packet
        .sources
        .iter()
        .map(|source| source.url.as_str())
        .collect();
    if operation == "rerank" {
        let urls: Vec<String> =
            serde_json::from_value(value.get("urls").cloned().ok_or("reranker omitted URLs")?)
                .map_err(|_| "reranker returned invalid URLs")?;
        let distinct: HashSet<&str> = urls.iter().map(String::as_str).collect();
        if urls.len() != packet.sources.len() || distinct != valid {
            return Err("reranker must return each evidence URL exactly once".into());
        }
        packet.sources.sort_by_key(|source| {
            urls.iter()
                .position(|url| url == &source.url)
                .unwrap_or(usize::MAX)
        });
    } else {
        let valid: HashSet<&str> = packet
            .sources
            .iter()
            .filter(|source| source.error.is_none() && !source.passages.is_empty())
            .map(|source| source.url.as_str())
            .collect();
        let answer = value
            .get("answer")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or("synthesizer omitted answer")?;
        let citations: Vec<String> = serde_json::from_value(
            value
                .get("citations")
                .cloned()
                .ok_or("synthesizer omitted citations")?,
        )
        .map_err(|_| "synthesizer returned invalid citations")?;
        if citations.is_empty() || citations.iter().any(|url| !valid.contains(url.as_str())) {
            return Err("synthesizer cited a source outside the evidence packet".into());
        }
        // Keep declared citations attached even when the generated prose omits
        // inline markers; the caller can audit all claimed grounding sources.
        packet.synthesis = Some(crate::core::fetch::sanitize_text(&format!(
            "{answer}\n\nSources:\n{}",
            citations.join("\n")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Admission waiting consumes the same processing budget and cannot start
    /// an extra child while another request owns the service's sole slot.
    #[tokio::test]
    async fn processor_admission_is_shared_and_deadline_bounded() {
        let config: crate::core::Config = serde_json::from_value(json!({
            "engines":{"use":[]},
            "processing":{"timeout":20,"max_concurrency":1,
                "synthesizer":{"command":"must-not-start-this-processor"}}
        }))
        .unwrap();
        let service = Search::open(config).unwrap();
        let permit = service.processing_permits.acquire().await.unwrap();
        let request = ResearchRequest {
            synthesize: true,
            ..Default::default()
        };
        let result = process(&service, &request, packet()).await.unwrap();
        assert_eq!(result.processing_errors, ["synthesize timed out"]);
        assert!(result.duration_ms >= 20);
        assert_eq!(service.processing_permits.available_permits(), 0);
        drop(permit);
        assert_eq!(service.processing_permits.available_permits(), 1);
    }

    /// Processing cannot reset a research deadline already consumed by retrieval.
    #[tokio::test]
    async fn exhausted_research_deadline_never_starts_processing() {
        let config: crate::core::Config = serde_json::from_value(json!({
            "engines":{"use":[]},
            "processing":{"synthesizer":{"command":"must-not-start-this-processor"}}
        }))
        .unwrap();
        let service = Search::open(config).unwrap();
        let request = ResearchRequest {
            synthesize: true,
            options: crate::core::ResearchOptions {
                timeout: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut evidence = packet();
        evidence.duration_ms = 10;
        let result = process(&service, &request, evidence).await.unwrap();
        assert_eq!(result.processing_errors, ["synthesize timed out"]);
        assert!(result.timed_out);
        assert!(result.incomplete);
        assert_eq!(result.sources.len(), 2);
    }

    /// Reranking and synthesis share one edge budget instead of each extending
    /// a slow request by another complete processor timeout.
    #[tokio::test]
    async fn processor_pipeline_shares_one_timeout() {
        let config: crate::core::Config = serde_json::from_value(json!({
            "engines":{"use":[]},
            "processing":{"timeout":20,
                "reranker":{"command":if cfg!(windows) {"python.exe"} else {"python3"},
                            "args":["-c","import time;time.sleep(10)"]},
                "synthesizer":{"command":"must-not-start-this-processor"}}
        }))
        .unwrap();
        let service = Search::open(config).unwrap();
        let request = ResearchRequest {
            synthesize: true,
            rerank: true,
            ..Default::default()
        };
        let result = process(&service, &request, packet()).await.unwrap();
        assert_eq!(
            result.processing_errors,
            ["rerank timed out", "synthesize timed out"]
        );
        assert!(result.duration_ms >= 20);
    }

    /// Absolute processors resolve relative resources beside their executable,
    /// just like command engines, unless the operator explicitly chooses cwd.
    #[cfg(unix)]
    #[tokio::test]
    async fn processor_working_directory_matches_command_engines() {
        let mut command: Command = serde_json::from_value(json!({
            "command":"/bin/sh",
            "args":["-c", "cat >/dev/null; printf '{\"cwd\":\"%s\"}' \"$PWD\""]
        }))
        .unwrap();
        let output = exchange(&command, "rerank", &packet(), 4096).await.unwrap();
        assert_eq!(
            std::path::Path::new(output["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            std::path::Path::new("/bin").canonicalize().unwrap()
        );
        command.cwd = Some(command.temp_dir.clone());
        let output = exchange(&command, "rerank", &packet(), 4096).await.unwrap();
        assert_eq!(
            std::path::Path::new(output["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            command.temp_dir.canonicalize().unwrap()
        );
    }

    /// A packet fixture with useful evidence and an explicitly failed attempt.
    fn packet() -> Packet {
        serde_json::from_value(json!({
            "queries": ["ownership"],
            "sources": [
                {"url":"https://example.com/useful", "engines":[],
                 "passages":[{"text":"Ownership rules.","score":1.0}]},
                {"url":"https://example.com/failed", "engines":[],
                 "passages":[], "error":"HTTP 404"}
            ],
            "engines":[], "coverage":{}, "characters":16, "duration_ms":0
        }))
        .unwrap()
    }

    /// A reranker cannot invent evidence or silently discard a failed attempt.
    #[test]
    fn reranking_requires_an_exact_permutation() {
        let mut packet = packet();
        assert!(apply(
            "rerank",
            json!({"urls":["https://example.com/invented"]}),
            &mut packet
        )
        .is_err());
        apply(
            "rerank",
            json!({"urls":["https://example.com/failed", "https://example.com/useful"]}),
            &mut packet,
        )
        .unwrap();
        assert_eq!(packet.sources[0].url, "https://example.com/failed");
        assert_eq!(packet.sources[1].passages[0].text, "Ownership rules.");
    }

    /// Citation membership is necessary, and failed attempts are not evidence.
    #[test]
    fn synthesis_requires_a_read_source_and_preserves_evidence() {
        let mut packet = packet();
        for citation in ["https://example.com/failed", "https://example.com/invented"] {
            assert!(apply(
                "synthesize",
                json!({"answer":"Claim", "citations":[citation]}),
                &mut packet
            )
            .is_err());
        }
        apply(
            "synthesize",
            json!({"answer":"Claim", "citations":["https://example.com/useful"]}),
            &mut packet,
        )
        .unwrap();
        assert!(packet
            .synthesis
            .as_deref()
            .unwrap()
            .contains("https://example.com/useful"));
        assert_eq!(packet.sources.len(), 2);
        assert_eq!(packet.sources[0].passages[0].text, "Ownership rules.");
    }

    /// Optional prose receives the same image-exfil neutralization as pages.
    #[test]
    fn generated_prose_cannot_restore_active_images() {
        let mut packet = packet();
        apply(
            "synthesize",
            json!({
                "answer":"A claim ![tracking](https://example.com/pixel?secret=data)",
                "citations":["https://example.com/useful"]
            }),
            &mut packet,
        )
        .unwrap();
        let answer = packet.synthesis.as_deref().unwrap();
        assert!(!answer.contains("!["));
        assert!(!answer.contains("pixel?secret"));
        assert!(answer.contains("https://example.com/useful"));
    }

    /// Output caps stop a processor before parsing oversized JSON.
    #[tokio::test]
    async fn processor_output_is_bounded() {
        let command: Command = serde_json::from_value(json!({
            "command": if cfg!(windows) { "python.exe" } else { "python3" },
            "args":["-c", "import json,sys;json.load(sys.stdin);print('x'*1000)"]
        }))
        .unwrap();
        let error = exchange(&command, "synthesize", &packet(), 10)
            .await
            .unwrap_err();
        assert_eq!(error, "processor output exceeded cap");
    }

    /// A slow optional processor leaves evidence intact and reports timeout.
    #[tokio::test]
    async fn processor_failure_does_not_destroy_the_packet() {
        let mut config: crate::core::Config = serde_json::from_value(json!({
            "engines":{"use":[]},
            "processing":{"timeout":20,"synthesizer":{
                "command":if cfg!(windows) {"python.exe"} else {"python3"},
                "args":["-c","import time;time.sleep(10)"]
            }}
        }))
        .unwrap();
        config.home = std::env::temp_dir().canonicalize().unwrap();
        let service = Search::open(config).unwrap();
        let request = ResearchRequest {
            synthesize: true,
            ..ResearchRequest::default()
        };
        let result = process(&service, &request, packet()).await.unwrap();
        assert_eq!(result.processing_errors, ["synthesize timed out"]);
        assert_eq!(result.sources[0].passages[0].text, "Ownership rules.");
        assert!(result.synthesis.is_none());
    }

    /// A configured processor remains dormant without request opt-in, then
    /// exchanges the actual packet and attaches only declared evidence citations.
    #[tokio::test]
    async fn synthesis_is_explicit_and_runs_at_the_edge() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-processing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let marker = root.join("called");
        let mut config: crate::core::Config = serde_json::from_value(json!({
            "engines":{"use":[]},
            "processing":{"synthesizer":{
                "command":if cfg!(windows) {"python.exe"} else {"python3"},
                "args":["-c", "import json,sys,pathlib;r=json.load(sys.stdin);assert r['operation']=='synthesize';pathlib.Path(r['config']['marker']).write_text('called');print(json.dumps({'answer':'Ownership rules.','citations':[r['packet']['sources'][0]['url']]}))"],
                "config":{"marker":marker}
            }}
        })).unwrap();
        config.home = root.clone();
        let service = Search::open(config).unwrap();
        let untouched = process(&service, &ResearchRequest::default(), packet())
            .await
            .unwrap();
        assert!(!marker.exists());
        assert!(untouched.synthesis.is_none());
        let request = ResearchRequest {
            synthesize: true,
            ..ResearchRequest::default()
        };
        let result = process(&service, &request, packet()).await.unwrap();
        assert!(marker.exists());
        assert!(result.processing_errors.is_empty());
        assert!(result
            .synthesis
            .as_deref()
            .unwrap()
            .contains("Ownership rules."));
        assert_eq!(result.sources[0].passages[0].text, "Ownership rules.");
        std::fs::remove_dir_all(root).unwrap();
    }
}
