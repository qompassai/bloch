use serde_json::Value;
use std::process::Command;

#[test]
fn manifest_verb_round_trips_without_a_repository() {
    let output = Command::new(env!("CARGO_BIN_EXE_bloch"))
        .args(["a-nonexistent-root", "manifest"])
        .output()
        .expect("run bloch manifest");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let manifest: Value = serde_json::from_slice(&output.stdout).expect("manifest JSON");
    assert_eq!(manifest["name"], "bloch");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["git_hash"], env!("BLOCH_EMBEDDED_GIT_HASH"));
    for (key, source) in [
        ("server_json", include_str!("../server.json")),
        ("agent_card", include_str!("../.well-known/agent-card.json")),
    ] {
        assert_eq!(
            manifest[key],
            serde_json::from_str::<Value>(source).unwrap()
        );
    }
}
