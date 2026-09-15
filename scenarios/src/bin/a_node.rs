//! Start one node on regtest and say how to reach it, then wait.
//!
//! Not a test — it asserts nothing and passes nothing. It is the fixture
//! [`dln-ctrl`](https://github.com/DarkWebDivingClub/nostr-rs-ln) needed
//! and nothing in this estate had: a real node, funded, with a channel,
//! that stays up while somebody types at it.
//!
//! ```text
//! cargo run --bin a_node
//! ```
//!
//! It prints the connection URIs and then holds them until interrupted,
//! at which point the node, its relay and its bitcoind all go away with
//! it. Nothing here survives the run, which is the point: every session
//! starts from a chain nobody has touched.
//!
//! **Two nodes, not one**, because half the interesting questions need a
//! counterparty — a route to price, a peer to open a channel with, an
//! invoice somebody else issued. The second one's URIs are printed too.

use std::path::PathBuf;

use anyhow::Result;
use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_harness::{relay, util};

/// Enough for a channel and change, mined before anything starts.
const BLOCKS: u64 = 110;
const CHANNEL_SATS: u64 = 1_000_000;
const PUSH_MSAT: u64 = 250_000;
/// A block every few seconds: fast enough that a channel confirms while
/// somebody is still looking at the terminal, slow enough to watch.
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let output_dir = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("a-node")),
    );
    std::fs::create_dir_all(&output_dir)?;

    // Every method, because the point of this fixture is to be typed at
    // and the narrow grant the suites use hides 13 of the 24.
    if std::env::var("DLN_E2E_FULL_GRANT").is_err() {
        std::env::set_var("DLN_E2E_FULL_GRANT", "1");
    }

    eprintln!("starting bitcoind, a relay and two nodes — about a minute");
    let chain = BitcoindHarness::start().await;
    let miner = chain.get_new_address().await;
    chain.mine_blocks(BLOCKS, &miner).await;

    let (_relay, relay_url) = relay::start_relay().await;

    let alice = DlnNode::start_on(
        "alice", SignerMode::Plain, None, &chain, &miner, &relay_url, &output_dir,
    )
    .await?;
    let bob = DlnNode::start_on(
        "bob", SignerMode::Plain, None, &chain, &miner, &relay_url, &output_dir,
    )
    .await?;

    // A channel, so `quote_payment` has a route to find and
    // `list_channels` has something to list. Confirmed here rather than
    // left pending, because a pending channel is the state that makes
    // every other answer confusing.
    eprintln!("opening a channel");
    let bob_id = bob.node_id();
    alice
        .open_channel(&bob_id, &format!("127.0.0.1:{}", bob.ln_port), CHANNEL_SATS, Some(PUSH_MSAT))
        .await?;
    for _ in 0..12 {
        chain.mine_blocks(1, &miner).await;
        if alice.has_ready_channel_with(&bob_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let ready = alice.has_ready_channel_with(&bob_id).await;

    println!("\n───────────────────────────────────────────────────────────");
    println!("  two nodes on regtest. channel ready: {ready}");
    println!("───────────────────────────────────────────────────────────\n");
    println!("# alice — has outbound, can pay");
    println!("export NWC_URI='{}'", alice.nwc_uri());
    println!();
    println!("# bob — has inbound, can be paid");
    println!("export BOB_NWC_URI='{}'", bob.nwc_uri());
    println!();
    // Two values, because an NNC URI carries no secret.
    let (nnc_uri, nnc_secret) = alice.nnc_uri();
    println!("# alice, node control — a different plane and a different grant");
    println!("export NNC_URI='{nnc_uri}'");
    println!("export NNC_SECRET='{nnc_secret}'");
    println!();
    println!("# bob's node id, for opening a channel at him");
    println!("export BOB_ID='{}'", bob.node_id());
    println!("export BOB_ADDR='127.0.0.1:{}'", bob.ln_port);
    println!();
    println!("# try:");
    println!("  dln-ctrl nwc get_info");
    println!("  dln-ctrl nwc get_balance");
    println!("  NWC_URI=$BOB_NWC_URI dln-ctrl nwc make_invoice '{{\"amount\":50000}}'");
    println!("  dln-ctrl nwc quote_payment '{{\"invoice\":\"lnbcrt…\"}}'");
    println!("  dln-ctrl nnc list_channels");
    println!("  dln-ctrl nnc list_peers");
    println!("\nlogs: {}", output_dir.display());
    println!("mining a block every {}s, so channels confirm", TICK.as_secs());
    println!("^C to stop — everything here goes with it\n");

    // **Keep mining.** Regtest produces nothing on its own, so a channel
    // opened while somebody is typing would sit unconfirmed forever and
    // `channel_opened` would never fire. A block every few seconds makes
    // this behave like a chain rather than a snapshot — which is what
    // anything driving it by hand expects.
    //
    // It also keeps this future alive, and the harness kills the child
    // processes when it drops.
    let mut blocks = 0u64;
    loop {
        tokio::time::sleep(TICK).await;
        chain.mine_blocks(1, &miner).await;
        blocks += 1;
        if blocks % 20 == 0 {
            eprintln!("(mined {blocks} blocks since start)");
        }
    }
}
