use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const HASH_OVERRIDE: &str = "BLOCH_GIT_HASH";
const DOCUMENTS: [(&str, &str); 2] = [
    ("server.json", "server.json"),
    (".well-known/agent-card.json", "agent-card.json"),
];

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    for (source, embedded) in DOCUMENTS {
        println!("cargo:rerun-if-changed={source}");
        fs::copy(source, out.join(embedded)).expect("copy embedded manifest document");
    }
    println!("cargo:rerun-if-env-changed={HASH_OVERRIDE}");
    let hash = env::var(HASH_OVERRIDE)
        .ok()
        .filter(|hash| valid_hash(hash))
        .or_else(worktree_hash);
    println!(
        "cargo:rustc-env=BLOCH_EMBEDDED_GIT_HASH={}",
        hash.as_deref().unwrap_or("unknown")
    );
}

fn valid_hash(hash: &str) -> bool {
    matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn worktree_hash() -> Option<String> {
    // Packaged sources can sit inside another repository.
    if !Path::new(".git").exists() {
        return None;
    }
    println!("cargo:rerun-if-changed=.git");
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    for reference in ["HEAD", "packed-refs"].into_iter().chain(branch.as_deref()) {
        if let Some(path) = git(&["rev-parse", "--git-path", reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    git(&["rev-parse", "--verify", "HEAD"]).filter(|hash| valid_hash(hash))
}
