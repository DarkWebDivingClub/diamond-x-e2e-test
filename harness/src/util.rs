use std::net::TcpListener;
use std::time::Duration;

/// Allocate a free ephemeral port by binding to port 0.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("read local addr").port();
    drop(listener);
    port
}

/// Create a unique temporary directory path based on current time.
pub fn unique_tmp_dir(prefix: &str) -> String {
    format!(
        "/tmp/{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos()
    )
}

/// Retry an async predicate until it returns true or the timeout expires.
pub async fn wait_until<F, Fut>(label: &str, timeout: Duration, interval: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if f().await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timeout waiting for: {label}");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Build `dln-node-knots` and return the binary path.
///
/// `KNOTS_NODE_DIR` overrides the checkout and `KNOTS_FEATURES` the cargo
/// features, so a scenario can be run against a node built with v2 header
/// support without editing it:
///
/// ```text
/// KNOTS_FEATURES=blake2b cargo run --bin knots_backend
/// ```
pub fn build_knots_node() -> anyhow::Result<String> {
    use anyhow::Context;

    let dir = std::path::PathBuf::from(
        std::env::var("KNOTS_NODE_DIR").unwrap_or_else(|_| KNOTS_NODE_DIR.to_string()),
    );
    anyhow::ensure!(dir.is_dir(), "{} not found", dir.display());

    let features = std::env::var("KNOTS_FEATURES").unwrap_or_default();
    let mut args = vec!["build", "--bin", "dln-node"];
    if !features.is_empty() {
        tracing::info!("building dln-node-knots with features: {features}");
        args.extend(["--features", features.as_str()]);
    }

    let status = std::process::Command::new("cargo")
        .args(&args)
        .current_dir(&dir)
        .status()
        .context("failed to run cargo build for dln-node-knots")?;
    anyhow::ensure!(status.success(), "cargo build failed with {status}");

    let binary = dir.join("target/debug/dln-node");
    anyhow::ensure!(binary.exists(), "binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}

/// Default checkout of the Knots build of the node.
pub const KNOTS_NODE_DIR: &str = "/home/rene/git/dln-node-knots";

/// Default checkout of the demo, which is where the party binaries live.
pub const DLN_X_DEMO_DIR: &str = "/home/rene/git/dln-x-demo";

/// Build `alice` and `bob` and return the directory holding them.
///
/// They are built rather than linked **on purpose**. A scenario that
/// called into a party would be a scenario that shared memory with it,
/// and the whole claim of the demo is that the two parties share nothing
/// but a relay.
pub fn build_party_binaries() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;

    let dir = std::path::PathBuf::from(
        std::env::var("DLN_X_DEMO_DIR").unwrap_or_else(|_| DLN_X_DEMO_DIR.to_string()),
    );
    anyhow::ensure!(dir.is_dir(), "{} not found", dir.display());

    tracing::info!("building the party binaries in {}", dir.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "alice", "--bin", "bob"])
        .current_dir(&dir)
        .status()
        .context("failed to run cargo build for the party binaries")?;
    anyhow::ensure!(status.success(), "cargo build failed with {status}");

    let out = dir.join("target/debug");
    for who in ["alice", "bob"] {
        anyhow::ensure!(out.join(who).exists(), "{who} not found in {}", out.display());
    }
    Ok(out)
}
