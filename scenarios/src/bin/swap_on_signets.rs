//! An atomic cross-chain swap, negotiated and settled by two processes
//! that share nothing but a relay.
//!
//! Missions 10.3 and 27. `swap_on_xbt` proves the swap against chains the
//! harness starts, premines and mines on demand. Here both legs run on the
//! live signets — `btc.signet.dwdc` and `xbt.signet.dwdc` — which the
//! harness cannot mine on. It ran against the public chains until
//! 2026-09-08; it now runs against the internal ones, which differ only
//! in being ours to reset.
//!
//! What that changes:
//!
//! - **No mining authority.** Every confirmation is waited for, at roughly
//!   sixty seconds each. The scenario asserts it cannot mine, so it cannot
//!   quietly regress into mining its way out of a wait.
//! - **Persistent state.** The chains are not reset between runs, so
//!   nothing here may assume a starting height, an empty wallet, or that it
//!   is the only thing that has ever happened.
//! - **Real funding.** Alice and Bob draw from the treasuries funded in
//!   10.1 and 10.2 rather than from a coinbase the test just made.
//!
//! The XBT chain has BLAKE2b active from height 1, so unlike `swap_on_xbt`
//! there is no activation to arrange — every block is already v2. The
//! header assertion stays, because a swap that passed with both legs on
//! v1 chains would prove nothing about XBT.
//!
//! ## This scenario does not perform the trade
//!
//! Mission 27. Until then this file held clients for all four nodes and
//! arranged both legs itself, which is fine for a test and **wrong for a
//! demo**: it showed a trade that could not happen between strangers,
//! because the thing arranging it could see both sides.
//!
//! Now it builds the stage and gets off it. It attaches to the chains,
//! starts four nodes, opens two channels — and then starts `alice` and
//! `bob`, two binaries from `dln-x-demo`, each handed the connection URIs
//! for **its own two nodes only** and the relay. They negotiate over
//! NIP-XZ and settle over NWC, and this process learns nothing about what
//! passed between them except from the nodes afterwards.
//!
//! **What it asserts on is chains and nodes.** A party that exited zero
//! while having done nothing must still fail the balance check, so a
//! party's own report of success is necessary and never sufficient.
//!
//! Happy path only, as in `swap_on_xbt`. Mission 04 is where abandonment
//! and CLTV ordering get tested.
//!
//! Run it deliberately, not in the suite:
//!
//! ```text
//! cargo run --bin swap_on_signets
//! ```
//!
//! It needs both treasuries funded, both signets reachable, and a checkout
//! of `dln-x-demo` (`DLN_X_DEMO_DIR`). See `doc/signet-treasury.md`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use tracing::info;

use dln_e2e_harness::bitcoind::{AttachConfig, BitcoindHarness};
use dln_e2e_harness::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_harness::{relay, util};

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;

/// Distinct per chain, so a balance assertion cannot pass by coincidence.
const CORE_LEG_MSAT: u64 = 120_000;
const KNOTS_LEG_MSAT: u64 = 250_000;

/// A block is about sixty seconds on both chains. Channel opens need
/// several, and a stalled miner must surface as a timeout naming the chain
/// rather than as a hang.
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(1200);

fn attach_cfg(port: u16, network: &str, label: &str) -> AttachConfig {
    AttachConfig {
        rpc_host: std::env::var("SIGNET_RPC_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        rpc_port: port,
        rpc_user: "treas".into(),
        rpc_password: std::env::var(if port == 48333 {
            "BTC_TREASURY_RPCPASS"
        } else {
            "XBT_TREASURY_RPCPASS"
        })
        .expect("treasury RPC password must be in the environment"),
        wallet: "treasury".into(),
        network: network.to_string(),
        label: label.to_string(),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run_scenario().await {
        Ok(()) => {
            println!("\n=== PASS ===");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

async fn run_scenario() -> Result<()> {
    let wall_clock = Instant::now();

    let output_dir = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("swap-on-signets")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    if std::env::var("KNOTS_FEATURES").is_err() {
        std::env::set_var("KNOTS_FEATURES", "blake2b");
    }
    let knots_binary = util::build_knots_node()?;
    anyhow::ensure!(
        std::env::var("DLN_NODE_BINARY").is_err(),
        "DLN_NODE_BINARY is set, which would put the same binary on both \
         chains and defeat the point of this scenario"
    );

    // ── Step 1: attach to two chains we do not own ──────────────────────
    info!("Step 1: attaching to the live signets");
    let core = BitcoindHarness::attach(attach_cfg(48333, "signet", "btc.signet")).await;
    let knots = BitcoindHarness::attach(attach_cfg(48332, "signet", "xbt.signet")).await;

    // The whole point of this scenario. If either of these could mine, it
    // would be swap_on_xbt with different hostnames.
    anyhow::ensure!(
        !core.can_mine() && !knots.can_mine(),
        "this scenario must hold no mining authority on either chain"
    );

    let core_height = height(&core).await?;
    let knots_height = height(&knots).await?;
    info!("  BTC signet at {core_height}, XBT signet at {knots_height} — neither is ours to mine");

    // Nothing below may assume a starting height; these are only reported.
    anyhow::ensure!(
        core_height > 0 && knots_height > 0,
        "a chain reported height 0 — is it still syncing?"
    );

    // ── Step 2: the chains really are different ─────────────────────────
    // XBT activated BLAKE2b at height 1, so its tip is v2 and always was.
    info!("Step 2: verifying the two chains use different header formats");
    assert_header(&core, core_height, 1)
        .await
        .context("the BTC chain should be v1")?;
    assert_header(&knots, knots_height, 2)
        .await
        .context("the XBT chain should be v2")?;
    assert_header(&knots, 1, 2)
        .await
        .context("XBT activates at height 1, so block 1 should already be v2")?;
    info!("  BTC v1 at {core_height}; XBT v2 at 1 and at {knots_height}");

    // ── Step 3: four nodes, funded from the treasuries ──────────────────
    info!("Step 3: four nodes — XBT side reads v2 headers, BTC side is stock");
    let (_relay_container, relay_url) = relay::start_relay().await;

    // On a chain we cannot mine, this address is never used; the treasury
    // pays and the scenario waits.
    let core_addr = core.get_new_address().await;
    let knots_addr = knots.get_new_address().await;

    let alice_core = DlnNode::start_on(
        "alice-core", SignerMode::Plain, None,
        &core, &core_addr, &relay_url, &output_dir,
    ).await.context("alice-core failed to start")?;
    let bob_core = DlnNode::start_on(
        "bob-core", SignerMode::Plain, None,
        &core, &core_addr, &relay_url, &output_dir,
    ).await.context("bob-core failed to start")?;
    let alice_knots = DlnNode::start_on(
        "alice-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_addr, &relay_url, &output_dir,
    ).await.context("alice-knots failed to start")?;
    let bob_knots = DlnNode::start_on(
        "bob-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_addr, &relay_url, &output_dir,
    ).await.context("bob-knots failed to start")?;
    info!("  four nodes funded from the treasuries after {:.0}s", wall_clock.elapsed().as_secs_f32());

    // ── Step 4: channels, confirmed by the chains' own miners ───────────
    let alice_core_id = alice_core.node_id();
    let bob_knots_id = bob_knots.node_id();
    bob_core
        .open_channel(&alice_core_id, &format!("127.0.0.1:{}", alice_core.ln_port),
                      CHANNEL_SATS, Some(PUSH_MSAT))
        .await.context("core channel open failed")?;
    alice_knots
        .open_channel(&bob_knots_id, &format!("127.0.0.1:{}", bob_knots.ln_port),
                      CHANNEL_SATS, Some(PUSH_MSAT))
        .await.context("knots channel open failed")?;

    info!("Step 4: waiting for both channels — real blocks, about a minute each");
    let opened = Instant::now();
    wait_for(CHANNEL_TIMEOUT, "both channels ready", || async {
        Ok(bob_core.has_ready_channel_with(&alice_core_id).await
            && alice_knots.has_ready_channel_with(&bob_knots_id).await)
    })
    .await
    .context("channels never confirmed — are both miners producing?")?;
    info!("  both channels ready after {:.0}s of waiting", opened.elapsed().as_secs_f32());

    // ── Step 5: the XBT channel really is on the BLAKE2b chain ──────────
    let funding_txid = alice_knots
        .list_channels()
        .await?
        .into_iter()
        .find(|c| c["peer_pubkey"].as_str() == Some(bob_knots_id.as_str()))
        .context("no XBT channel")?["funding_txid"]
        .as_str()
        .context("XBT channel has no funding_txid")?
        .to_string();
    let funding_height = tx_block_height(&knots, &funding_txid).await?;
    assert_header(&knots, funding_height, 2).await?;
    info!("Step 5: XBT funding {funding_txid} confirmed in block {funding_height}, a v2 block");

    // ── Step 6: hand each party its own two nodes, and nothing else ────
    //
    // This is where the scenario stops being a participant. Everything
    // above is stagecraft — chains, nodes, channels — and everything
    // below is two strangers with a relay between them.
    info!("Step 6: starting alice and bob as separate processes");
    let party_bin = util::build_party_binaries()?;

    // Balances read *before* either party runs, from the nodes rather
    // than from anything a party will later claim.
    let alice_before = alice_core.get_balance_msat().await?;
    let bob_before = bob_knots.get_balance_msat().await?;

    // Trade-plane identities. Each party gets its own secret and neither
    // gets the other's — they find each other by the offer, which is how
    // a taker finds a maker with no introduction.
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    // Bob quotes a price. Alice does not know it until she reads the
    // offer, and this scenario does not tell her: `PRICE_PPM` goes to Bob
    // alone, and `expected_xbt` below is what the orchestrator derives so
    // it can check the outcome — not something Alice is given.
    let price_ppm: u64 = (KNOTS_LEG_MSAT as u128 * 1_000_000 / CORE_LEG_MSAT as u128) as u64;
    let expected_xbt = CORE_LEG_MSAT as u128 * price_ppm as u128;
    let expected_xbt = ((expected_xbt + 999_999) / 1_000_000) as u64;

    let mut bob = std::process::Command::new(party_bin.join("bob"));
    bob.env("BOB_BTC_URI", bob_core.nwc_uri())
        .env("BOB_XBT_URI", bob_knots.nwc_uri())
        .env("BOB_RELAY", &relay_url)
        .env("BOB_NOSTR_SECRET", bob_keys.secret_key().to_secret_hex())
        .env("BOB_PRICE_PPM", price_ppm.to_string())
        .stdout(log_file(&output_dir, "bob.log")?)
        .stderr(log_file(&output_dir, "bob.err")?);

    let mut alice = std::process::Command::new(party_bin.join("alice"));
    alice
        .env("ALICE_BTC_URI", alice_core.nwc_uri())
        .env("ALICE_XBT_URI", alice_knots.nwc_uri())
        .env("ALICE_RELAY", &relay_url)
        .env("ALICE_NOSTR_SECRET", alice_keys.secret_key().to_secret_hex())
        .env("ALICE_AMOUNT_MSAT", CORE_LEG_MSAT.to_string())
        .stdout(log_file(&output_dir, "alice.log")?)
        .stderr(log_file(&output_dir, "alice.err")?);

    // Nothing above hands either command a node of the other's, a path
    // into this process, or a file the other writes. The isolation is a
    // property of this block and can be read off it.

    // ── Step 7: let them trade ─────────────────────────────────────────
    //
    // Bob first, because a taker cannot find an offer that has not been
    // published. That is the only ordering between them, and it is
    // startup ordering rather than protocol: Alice waits sixty seconds
    // for an offer, so a slow Bob costs time and not a failure.
    info!("Step 7: bob publishes, alice takes — this process now only waits");
    let traded = Instant::now();
    let mut bob = bob.spawn().context("failed to start bob")?;
    let mut alice = alice.spawn().context("failed to start alice")?;

    let alice_status = wait_for_exit(&mut alice, "alice", Duration::from_secs(600)).await;
    let bob_status = wait_for_exit(&mut bob, "bob", Duration::from_secs(600)).await;

    // Reported before either is checked, so a run where both failed says
    // so about both.
    info!(
        "  alice {:?}, bob {:?}, after {:.1}s — logs in {}",
        alice_status.as_ref().map(|s| s.to_string()),
        bob_status.as_ref().map(|s| s.to_string()),
        traded.elapsed().as_secs_f32(),
        output_dir.display()
    );
    let alice_status = alice_status?;
    let bob_status = bob_status?;
    anyhow::ensure!(alice_status.success(), "alice exited {alice_status}");
    anyhow::ensure!(bob_status.success(), "bob exited {bob_status}");

    // ── Step 8: the nodes agree that it happened ───────────────────────
    //
    // Both parties claim success. That is not the evidence — this is.
    info!("Step 8: verifying balances on both chains");
    wait_for(Duration::from_secs(120), "balances to settle", || async {
        Ok(alice_core.get_balance_msat().await? >= alice_before + CORE_LEG_MSAT
            && bob_knots.get_balance_msat().await? >= bob_before + expected_xbt)
    })
    .await
    .context(
        "both parties exited zero and the balances did not move — \
         a party reporting success is not a trade",
    )?;

    let alice_after = alice_core.get_balance_msat().await?;
    let bob_after = bob_knots.get_balance_msat().await?;
    info!("  alice on BTC:  {alice_before} -> {alice_after} msat");
    info!("  bob on XBT:    {bob_before} -> {bob_after} msat (quoted at {price_ppm} ppm)");

    // Atomicity, stated as the thing that would have been observed had it
    // failed: one leg moving without the other.
    anyhow::ensure!(
        alice_after > alice_before && bob_after > bob_before,
        "one leg moved and the other did not — the swap was not atomic"
    );

    info!(
        "  total wall-clock {:.0}s, of which {:.0}s was waiting for channels \
         and {:.0}s was the trade",
        wall_clock.elapsed().as_secs_f32(),
        opened.elapsed().as_secs_f32(),
        traded.elapsed().as_secs_f32()
    );

    Ok(())
}

/// A file for a child's output, kept beside the node logs.
///
/// A party's stdout is **evidence of what it did**, not input to the
/// assertions. Nothing here reads these back.
fn log_file(dir: &std::path::Path, name: &str) -> Result<std::fs::File> {
    std::fs::File::create(dir.join(name))
        .with_context(|| format!("could not create {name}"))
}

/// Wait for a child, killing it if it outstays the deadline.
///
/// A party that hangs must surface as a named failure rather than as a
/// scenario that never returns.
async fn wait_for_exit(
    child: &mut std::process::Child,
    who: &str,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{who} did not finish within {}s", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn height(bitcoind: &BitcoindHarness) -> Result<u64> {
    bitcoind
        .rpc("getblockcount", serde_json::json!([]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockcount failed: {e}"))?
        .as_u64()
        .context("getblockcount did not return a number")
}
/// Assert a block's header format, from the raw serialization and from
/// the field the RPC reports, so the two cross-check.
async fn assert_header(bitcoind: &BitcoindHarness, height: u64, want: u64) -> Result<()> {
    use serde_json::json;

    let hash = bitcoind
        .rpc("getblockhash", json!([height]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockhash({height}) failed: {e}"))?;

    let verbose = bitcoind
        .rpc("getblockheader", json!([hash]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockheader({height}) failed: {e}"))?;
    // Knots reports the format in "header_version": 2 for a v2 header and
    // **0** for a v1 one — not 1. A node predating the field omits it, which
    // also means v1. Callers still say 1 or 2, because "v1" reads better than
    // "0" at the call site.
    let reported = verbose["header_version"].as_u64().unwrap_or(0);
    let expected = if want == 2 { 2 } else { 0 };
    anyhow::ensure!(
        reported == expected,
        "height {height}: header_version is {reported}, expected {expected} (v{want})"
    );

    let hex = bitcoind
        .rpc("getblockheader", json!([hash, false]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockheader({height}, false) failed: {e}"))?;
    let raw = hex::decode(hex.as_str().context("header not a string")?)?;
    let (want_len, want_bit31) = if want == 2 { (164, true) } else { (80, false) };
    anyhow::ensure!(
        raw.len() == want_len,
        "height {height}: header is {} bytes, expected {want_len}",
        raw.len()
    );
    let version = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    anyhow::ensure!(
        (version & 0x8000_0000 != 0) == want_bit31,
        "height {height}: nVersion is 0x{version:08x}, which disagrees with header_version {reported}"
    );
    Ok(())
}

/// The height of the block a transaction was confirmed in.
async fn tx_block_height(bitcoind: &BitcoindHarness, txid: &str) -> Result<u64> {
    use serde_json::json;

    let tx = bitcoind
        .rpc("getrawtransaction", json!([txid, true]))
        .await
        .map_err(|e| anyhow::anyhow!("getrawtransaction({txid}) failed: {e}"))?;
    let blockhash = tx["blockhash"].as_str().context("transaction is unconfirmed")?;
    let header = bitcoind
        .rpc("getblockheader", json!([blockhash]))
        .await
        .map_err(|e| anyhow::anyhow!("getblockheader failed: {e}"))?;
    header["height"].as_u64().context("no height in the header")
}

async fn wait_for<F, Fut>(timeout: Duration, what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
