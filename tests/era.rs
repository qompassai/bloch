//! Era gates (WS2, D-113): the migrated server speaks the modern
//! MCP era only (2026-07-28). Legacy contact is refused with the
//! designed typed errors and the server survives every refusal;
//! framing failures follow the fleet guard contract (see
//! `bloch::guard`). Frames are raw strings and responses are
//! correlated by JSON-RPC id (rmcp dispatches pipelined requests
//! concurrently).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const META: &str = r#""_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "era-test", "version": "0"}, "io.modelcontextprotocol/clientCapabilities": {}}"#;
const LEGACY_META: &str = r#""_meta": {"io.modelcontextprotocol/protocolVersion": "2025-06-18", "io.modelcontextprotocol/clientInfo": {"name": "era-test", "version": "0"}, "io.modelcontextprotocol/clientCapabilities": {}}"#;

fn bloch_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bloch")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bloch-era-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A minimal serve root: one repo with an indexed memory file and
/// a tracker, so tools/call smokes have real content to return.
fn fixture_root(base: &std::path::Path) -> PathBuf {
    let repo = base.join("repo_a");
    let mem = repo.join(".ai").join("memory");
    std::fs::create_dir_all(mem.join("inbox")).expect("fixture dirs");
    std::fs::write(
        mem.join("decisions.md"),
        "# Decisions\n\nThe widget sprocket decision: use sprockets everywhere.\n",
    )
    .expect("fixture decisions");
    std::fs::write(
        repo.join("CHECKLIST.md"),
        "---\ntracker_generation: 3\n---\n# Checklist\n\n## Core\n- [x] Sprockets wired memory:decisions.md\n- [ ] Open item\n",
    )
    .expect("fixture checklist");
    repo
}

struct RawServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RawServer {
    fn spawn(root: &std::path::Path) -> RawServer {
        let mut child = Command::new(bloch_bin())
            .arg("serve")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bloch server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        RawServer {
            child,
            stdin,
            stdout,
        }
    }

    /// Send raw lines; read exactly `expect` frames. Returns them
    /// in arrival order (callers correlate by id).
    fn exchange(&mut self, lines: &[String], expect: usize) -> Vec<serde_json::Value> {
        for line in lines {
            self.stdin.write_all(line.as_bytes()).expect("write frame");
            self.stdin.write_all(b"\n").expect("write newline");
        }
        self.stdin.flush().expect("flush");
        let mut frames = Vec::new();
        for _ in 0..expect {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read frame");
            assert!(read > 0, "server closed stdout unexpectedly");
            frames.push(serde_json::from_str(&line).expect("server frame is JSON"));
        }
        frames
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "server exit not clean: {status}");
                    return;
                }
                Ok(None) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "server did not exit on EOF"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => panic!("wait failed: {e}"),
            }
        }
    }
}

fn by_id(frames: &[serde_json::Value], id: i64) -> &serde_json::Value {
    frames
        .iter()
        .find(|f| f["id"] == serde_json::json!(id))
        .unwrap_or_else(|| panic!("no frame with id {id} in {frames:?}"))
}

fn discover(id: i64) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"server/discover","params":{{{META}}}}}"#)
}

fn initialize(id: i64, version: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"era-test","version":"0"}}}}}}"#
    )
}

#[test]
fn discover_advertises_only_the_modern_era() {
    let base = temp_dir("discover");
    let root = fixture_root(&base);
    let mut srv = RawServer::spawn(&root);
    let frames = srv.exchange(&[discover(1)], 1);
    let result = &by_id(&frames, 1)["result"];
    assert_eq!(
        result["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "bloch"
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    srv.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn legacy_initialize_is_refused_and_the_server_survives() {
    let base = temp_dir("init");
    let root = fixture_root(&base);
    let mut srv = RawServer::spawn(&root);
    for (id, version) in [(1, "2024-11-05"), (2, "2025-06-18"), (3, "2026-07-28")] {
        let frames = srv.exchange(&[initialize(id, version)], 1);
        let frame = by_id(&frames, id);
        assert_eq!(frame["error"]["code"], -32022, "{frame}");
        assert_eq!(frame["error"]["data"]["requested"], version);
        assert_eq!(
            frame["error"]["data"]["supported"],
            serde_json::json!(["2026-07-28"])
        );
    }
    // The refusals are not teardowns: a modern session still works.
    let frames = srv.exchange(&[discover(4)], 1);
    assert_eq!(
        by_id(&frames, 4)["result"]["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    srv.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn tools_list_requires_modern_meta() {
    let base = temp_dir("meta");
    let root = fixture_root(&base);
    let mut srv = RawServer::spawn(&root);
    // Legacy _meta: the SDK's per-request version gate refuses.
    let legacy =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{LEGACY_META}}}}}"#);
    let frames = srv.exchange(&[legacy], 1);
    let frame = by_id(&frames, 1);
    assert_eq!(frame["error"]["code"], -32022, "{frame}");
    assert_eq!(
        frame["error"]["data"]["supported"],
        serde_json::json!(["2026-07-28"])
    );
    // Modern _meta: the full 11-tool surface, in definition order.
    let modern = format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{META}}}}}"#);
    let frames = srv.exchange(&[modern], 1);
    let tools = by_id(&frames, 2)["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "memory_search",
            "memory_get",
            "memory_propose",
            "memory_promote",
            "tracker_state",
            "tracker_rollup",
            "idea_lookup",
            "idea_register",
            "lessons_search",
            "roadmap_state",
            "roadmap_rollup"
        ]
    );
    srv.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn framing_errors_follow_the_guard_contract() {
    let base = temp_dir("framing");
    let root = fixture_root(&base);
    let mut srv = RawServer::spawn(&root);
    let frames = srv.exchange(
        &[
            "this is not json".to_string(),
            "[1,2,3]".to_string(),
            "42".to_string(),
        ],
        3,
    );
    assert_eq!(frames[0]["error"]["code"], -32700);
    assert!(
        frames[0]["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("parse error: "),
        "{frames:?}"
    );
    assert_eq!(frames[1]["error"]["code"], -32600);
    assert_eq!(frames[2]["error"]["code"], -32600);
    // The server survives its own framing errors.
    let frames = srv.exchange(&[discover(9)], 1);
    assert_eq!(
        by_id(&frames, 9)["result"]["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    srv.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn notifications_are_dropped_unexecuted() {
    let base = temp_dir("notif");
    let root = fixture_root(&base);
    let mut srv = RawServer::spawn(&root);
    // A notification-shaped tools/call for a write-class tool,
    // then a discover: exactly one frame comes back (the
    // discover response), and no inbox file was created.
    let notification = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"memory_propose","arguments":{"repo":"repo_a","agent":"era-test","topic":"sneaky","content":"filed by notification","generation":3}}}"#
        .to_string();
    let frames = srv.exchange(&[notification, discover(5)], 1);
    assert_eq!(
        by_id(&frames, 5)["result"]["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    let inbox = root.join(".ai").join("memory").join("inbox");
    let filed: Vec<_> = std::fs::read_dir(&inbox)
        .expect("inbox dir")
        .filter_map(|e| e.ok())
        .collect();
    assert!(filed.is_empty(), "notification executed a write: {filed:?}");
    srv.shutdown();
    let _ = std::fs::remove_dir_all(&base);
}
