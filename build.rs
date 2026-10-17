/// Inject the current git short hash into the binary as APP_VERSION.
///
/// When built inside a git repository the version string becomes
/// `<semver>+<hash>` (e.g. `1.11.1+a3b4c5d`).  When git is unavailable
/// (Docker image builds copy only src/ without .git) the plain semver
/// from Cargo.toml is used.
fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let full = if hash.is_empty() {
        version
    } else {
        format!("{version}+{hash}")
    };
    println!("cargo:rustc-env=APP_VERSION={full}");
    // Re-run when HEAD moves so the hash stays fresh. Two watch points are
    // needed:
    //   - .git/HEAD changes on a branch switch / checkout, but NOT on a plain
    //     commit (its content stays "ref: refs/heads/main"), which is exactly
    //     why the hash went stale after commits.
    //   - .git/logs/HEAD (the reflog) is appended on every commit/checkout, so
    //     its mtime changes on each commit — this is the reliable trigger.
    // In a Docker build there is no .git, so these paths don't exist; Cargo
    // then re-runs this script every build (harmless: git fails, hash is empty,
    // and the plain semver from Cargo.toml is used).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
}
