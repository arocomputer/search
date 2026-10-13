//! Offline engine CLI contracts. Every subprocess owns its home and settings,
//! and an unusable selected remote proves engine commands stay local.

#[cfg(windows)]
#[path = "../support/windows.rs"]
mod windows;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// Isolated home, settings, and a command engine program for one test.
struct Fixture {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

/// Human rendering strips active controls without changing the JSON response.
#[test]
fn search_terminal_controls_are_neutralized_only_in_human_output() {
    let fixture = Fixture::new(
        r#"import json, sys
text = 'untrusted\x1b]52;c;data\x07\x1b[2J\u202e'
json.dump({'results': [{'title': text, 'url': 'https://example.com/', 'snippet': text}]}, sys.stdout)
"#,
    );
    fixture.success(&["enable", "fixture"]);
    fixture.success(&["remote", "off"]);
    let human = fixture.success(&["offline query"]);
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("untrusted]52;c;data[2J"));
    assert!(!text.contains(['\x1b', '\x07', '\u{202e}']));
    let json = fixture.success(&["offline query", "--json"]);
    let answer: Value = serde_json::from_slice(&json.stdout).unwrap();
    for field in ["title", "snippet"] {
        assert_eq!(
            answer["results"][0][field],
            "untrusted\x1b]52;c;data\x07\x1b[2J\u{202e}"
        );
    }
    let invalid = fixture.run(&["offline query", "--config", "missing\x1b[2J.json"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(!invalid.stderr.contains(&b'\x1b'));
}

impl Fixture {
    /// Write settings defining (but not selecting) command engine `fixture`
    /// that runs `script` from its own directory.
    fn new(script: &str) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-engines-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let trust = home.join("trust");
        let engine = root.join("engine");
        for dir in [&root, &home, &trust, &engine] {
            fs::create_dir(dir).unwrap();
        }
        fs::write(engine.join("program.py"), script).unwrap();
        let config = home.join("settings.json");
        fs::write(
            &config,
            serde_json::to_vec(&json!({
                "notes": {"owner": "keep this note"},
                "search": {"timeout": 3000},
                "engines": {"use": [], "config": {"fixture": {
                    "type": "command",
                    "command": if cfg!(windows) { "python.exe" } else { "python3" },
                    "args": ["program.py"],
                    "cwd": engine
                }}}
            }))
            .unwrap(),
        )
        .unwrap();
        // An unusable selected remote makes accidental remote routing fail.
        fs::write(
            trust.join("remotes.json"),
            r#"{"selected":"missing","remotes":{}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&root, &home, &trust] {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
            }
            for file in [&config, &trust.join("remotes.json")] {
                fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        #[cfg(windows)]
        for path in [&root, &home, &config, &trust, &trust.join("remotes.json")] {
            windows::secure(path);
        }
        Self { root, home, config }
    }

    /// Keep environment changes in children, including credential variables for env tests.
    fn process(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_search"));
        command
            .args(args)
            .env("SEARCH_HOME", &self.home)
            .env_remove("CONFIG")
            .env_remove("ADDRESS")
            .env("HOME", &self.root)
            .env("FIXTURE_DECLARED", "declared-value")
            .env("FIXTURE_UNDECLARED", "hidden-value")
            .current_dir(&self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.process(args).output().unwrap()
    }

    /// Assert success with actionable process output when the contract breaks.
    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{output:?}");
        output
    }

    fn settings(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Terminal output must not disguise a broken engine as a successful empty search.
#[test]
fn text_search_reports_engine_failures_without_leaking_program_stderr() {
    let fixture =
        Fixture::new("import sys\nprint('PRIVATE_MARKER', file=sys.stderr)\nsys.exit(1)\n");
    fixture.success(&["enable", "fixture"]);
    fixture.success(&["remote", "off"]);
    let output = fixture.run(&["offline query"]);
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("fixture: failed"), "{diagnostic}");
    assert!(!diagnostic.contains("PRIVATE_MARKER"));
}

/// `test` runs one unselected engine with the command protocol: literal
/// query, settings `config`, declared environment only, and its own cwd.
#[test]
fn test_runs_one_engine_with_declared_env_config_and_cwd() {
    let fixture = Fixture::new(
        r#"
import json, os, sys
from pathlib import Path
assert Path('program.py').is_file()
request = json.load(sys.stdin)
assert request['query'] == '-two words'
assert request['limit'] == 10
assert request['config'] == {'label': 'value'}
assert os.environ['FIXTURE_DECLARED'] == 'declared-value'
assert 'FIXTURE_UNDECLARED' not in os.environ
json.dump({'results':[{'title':'Fixture','url':'https://example.com/page'},{'title':'','url':'https://example.com/bad'}]}, sys.stdout)
"#,
    );
    fixture.success(&["configure", "fixture", "config", r#"{"label":"value"}"#]);
    fixture.success(&["configure", "fixture", "env", r#"["FIXTURE_DECLARED"]"#]);
    let tested = fixture.success(&["test", "fixture", "-json", "--", "-two", "words"]);
    let answer: Value = serde_json::from_slice(&tested.stdout).unwrap();
    assert_eq!(answer["results"][0]["engines"], json!(["fixture"]));
    assert_eq!(answer["engines"][0]["status"], "ok");
    assert_eq!(answer["engines"][0]["skipped"], 1);
    let text = fixture.success(&["engines", "test", "fixture", "--", "-two", "words"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("1. Fixture"));
    assert!(String::from_utf8_lossy(&text.stderr).contains("skipped 1 invalid results"));
    assert_eq!(fixture.settings()["engines"]["use"], json!([]));
}

/// A failing engine test exits nonzero without leaking program stderr.
#[test]
fn failed_test_exits_nonzero_without_leaking_stderr() {
    let fixture =
        Fixture::new("import sys; print('credential-secret',file=sys.stderr); sys.exit(3)");
    let tested = fixture.run(&["test", "fixture", "-json", "query"]);
    assert!(!tested.status.success());
    let answer: Value = serde_json::from_slice(&tested.stdout).unwrap();
    assert_eq!(answer["engines"][0]["status"], "error");
    for stream in [&tested.stdout, &tested.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("credential-secret"));
    }
    assert!(!fixture.run(&["test", "unknown", "query"]).status.success());
}

/// Selection edits `engines.use` only, keeping unrelated settings, and refuses
/// an engine that cannot run with the command that fixes it.
#[test]
fn enable_requires_a_ready_engine_and_preserves_settings() {
    let fixture = Fixture::new("");
    let refused = fixture.run(&["enable", "searxng"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("search configure searxng url URL"));
    assert!(!fixture.run(&["enable", "undefined"]).status.success());
    fixture.success(&[
        "configure",
        "searxng",
        "url",
        "https://searx.example/search",
    ]);
    fixture.success(&["enable", "searxng"]);
    fixture.success(&["enable", "fixture"]);
    assert_eq!(
        fixture.settings()["engines"]["use"],
        json!(["searxng", "fixture"])
    );
    fixture.success(&["disable", "searxng"]);
    assert_eq!(fixture.settings()["engines"]["use"], json!(["fixture"]));
    assert_eq!(fixture.settings()["notes"]["owner"], "keep this note");
    assert_eq!(fixture.settings()["search"]["timeout"], 3000);
    assert_eq!(
        fixture.settings()["engines"]["config"]["searxng"],
        json!({"url": "https://searx.example/search"})
    );
}

/// Deselecting is always possible, even when the selected engine is broken.
#[test]
fn disable_switches_off_a_broken_selected_engine() {
    let fixture = Fixture::new("");
    fs::write(&fixture.config, r#"{"engines":{"use":["mwmbl","broken"]}}"#).unwrap();
    assert!(!fixture.run(&["enable", "mwmbl"]).status.success());
    fixture.success(&["disable", "broken"]);
    assert_eq!(fixture.settings()["engines"]["use"], json!(["mwmbl"]));
}

/// Built-in types are fixed, and errors name fields without echoing values.
#[test]
fn configure_refuses_type_changes_and_never_echoes_values() {
    let fixture = Fixture::new("");
    fixture.success(&["enable", "fixture"]);
    for args in [
        vec!["configure", "searxng", "type", "command"],
        vec![
            "configure",
            "fixture",
            "max_response_bytes",
            "credential-secret",
        ],
        vec![
            "configure",
            "fixture",
            "env",
            r#"["TOKEN=credential-secret"]"#,
        ],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        for stream in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains("credential-secret"));
        }
    }
    let shown = fixture.success(&["configure", "searxng"]);
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.contains("\"format\": \"json\""), "{shown}");
    assert!(shown.contains("required field url is missing"), "{shown}");
}

/// The listing covers presets, custom engines and dangling selections.
#[test]
fn list_reports_builtin_custom_and_selection_status() {
    let fixture = Fixture::new("");
    fs::write(
        &fixture.config,
        serde_json::to_vec(&json!({"engines": {
            "use": ["mwmbl", "ghost"],
            "config": {"company": {"type": "http", "url": "https://search.example.com/api"}}
        }}))
        .unwrap(),
    )
    .unwrap();
    let listed = fixture.success(&["engines", "-json"]);
    let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let row = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(row("mwmbl")["selected"], true);
    assert_eq!(row("mwmbl")["status"], "ready");
    assert_eq!(row("searxng")["builtin"], true);
    assert!(row("searxng")["status"]
        .as_str()
        .unwrap()
        .contains("search configure searxng url URL"));
    assert_eq!(row("company")["builtin"], false);
    assert_eq!(row("company")["type"], "http");
    assert_eq!(row("ghost")["selected"], true);
    assert!(row("ghost")["status"]
        .as_str()
        .unwrap()
        .contains("not configured"));
    let text = fixture.success(&["engines"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("* mwmbl"));
}

/// Concurrent writers serialize under the lock, and an explicit settings file
/// leaves the home settings untouched.
#[test]
fn settings_updates_serialize_on_an_explicit_file() {
    let fixture = Fixture::new("");
    let alternate = fixture.root.join("alternate.json");
    fs::copy(&fixture.config, &alternate).unwrap();
    #[cfg(windows)]
    windows::secure(&alternate);
    let before = fs::read(&fixture.config).unwrap();
    let mut children = Vec::new();
    for (key, value) in [
        ("env", r#"["FIXTURE_DECLARED"]"#),
        ("config", r#"{"keep":true}"#),
    ] {
        children.push(
            fixture
                .process(&["-config"])
                .arg(&alternate)
                .args(["configure", "fixture", key, value])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let settings: Value = serde_json::from_slice(&fs::read(&alternate).unwrap()).unwrap();
    let entry = &settings["engines"]["config"]["fixture"];
    assert_eq!(entry["env"], json!(["FIXTURE_DECLARED"]));
    assert_eq!(entry["config"], json!({"keep": true}));
    assert_eq!(fs::read(&fixture.config).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn settings_and_lock_symlinks_are_rejected_without_writing_the_target() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("");
    let target = fixture.root.join("target.json");
    fs::rename(&fixture.config, &target).unwrap();
    symlink(&target, &fixture.config).unwrap();
    assert!(!fixture.run(&["disable", "mwmbl"]).status.success());
    fs::remove_file(&fixture.config).unwrap();
    fs::rename(&target, &fixture.config).unwrap();
    fixture.success(&["disable", "mwmbl"]);
    let lock = fixture.home.join("settings.json.lock");
    fs::remove_file(&lock).unwrap();
    symlink(&fixture.config, &lock).unwrap();
    let before = fs::read(&fixture.config).unwrap();
    assert!(!fixture.run(&["disable", "mwmbl"]).status.success());
    assert_eq!(fs::read(&fixture.config).unwrap(), before);
}

/// Writes create owner-only files and directories, keep an existing parent's
/// mode, and refuse settings others can write.
#[cfg(unix)]
#[test]
fn settings_writes_are_private_and_refuse_shared_writable_files() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let fixture = Fixture::new("");
    let parent = fixture.root.join("shared-settings");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let config = parent.join("settings.json");
    fs::copy(&fixture.config, &config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let output = fixture
        .process(&["disable", "mwmbl"])
        .env("CONFIG", &config)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(mode(&parent), 0o755);
    assert_eq!(mode(&config), 0o600);
    assert_eq!(mode(&parent.join("settings.json.lock")), 0o600);
    for (path, unsafe_mode, safe_mode) in [(&config, 0o666, 0o600), (&parent, 0o777, 0o755)] {
        fs::set_permissions(path, fs::Permissions::from_mode(unsafe_mode)).unwrap();
        let refused = fixture
            .process(&["disable", "fixture"])
            .env("CONFIG", &config)
            .output()
            .unwrap();
        assert!(!refused.status.success());
        fs::set_permissions(path, fs::Permissions::from_mode(safe_mode)).unwrap();
    }
    fs::remove_dir_all(&fixture.home).unwrap();
    fixture.success(&["disable", "mwmbl"]);
    assert_eq!(mode(&fixture.home), 0o700);
    assert_eq!(mode(&fixture.config), 0o600);
}

/// A Git Bash HOME must not redirect native Windows's implicit settings file.
#[cfg(windows)]
#[test]
fn implicit_windows_home_uses_userprofile_and_explicit_home_still_wins() {
    let fixture = Fixture::new("");
    let profile = fixture.root.join("profile");
    fs::create_dir(&profile).unwrap();
    windows::secure(&profile);
    let output = fixture
        .process(&["disable", "mwmbl"])
        .env_remove("SEARCH_HOME")
        .env("USERPROFILE", &profile)
        .env("HOME", "/git-bash-home")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(profile.join(".search/settings.json").is_file());
    let explicit = fixture.root.join("explicit");
    let output = fixture
        .process(&["disable", "mwmbl"])
        .env("SEARCH_HOME", &explicit)
        .env("USERPROFILE", "relative-invalid-profile")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(explicit.join("settings.json").is_file());
}

/// Settings come only from `$SEARCH_HOME/settings.json`: a settings file in the working
/// directory cannot define a command engine that then runs.
#[test]
fn working_directory_settings_never_define_engines() {
    let fixture = Fixture::new("");
    let marker = fixture.root.join("ran");
    fs::write(
        fixture.root.join("settings.json"),
        serde_json::to_vec(&json!({"engines": {
            "use": ["rogue"],
            "config": {"rogue": {"type": "command", "command": "/bin/sh", "args": ["-c", format!("touch {}", marker.display())]}}
        }}))
        .unwrap(),
    )
    .unwrap();
    let listed = fixture.success(&["engines", "-json"]);
    let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["id"] != "rogue"));
    fixture.run(&["--", "hello"]);
    assert!(!marker.exists());
}

/// `search research` prints one JSON evidence packet and reports honest
/// coverage even when the source cannot be read.
#[test]
fn research_command_prints_one_packet() {
    let fixture = Fixture::new(
        "import json,sys; json.load(sys.stdin); json.dump({'results':[{'title':'T','url':'http://127.0.0.1:1/x','published':'2026-01-01'}]},sys.stdout)",
    );
    let mut settings = fixture.settings();
    settings["fetch"] = json!({"allow_private_networks": true});
    fs::write(&fixture.config, serde_json::to_vec(&settings).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.success(&["enable", "fixture"]);
    fixture.success(&["remote", "off"]);
    let output = fixture.success(&["research", "needle", "-json"]);
    let packet: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(packet["queries"][0], "needle");
    assert_eq!(packet["coverage"]["candidates"], 1);
    assert_eq!(packet["coverage"]["failed"], 1);
    assert!(fixture.run(&["research"]).status.code() == Some(2));
}
