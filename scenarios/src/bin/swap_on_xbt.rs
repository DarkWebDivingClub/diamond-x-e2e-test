//! An atomic cross-chain swap with a XBT leg that is really XBT.
//!
//! Mission 03.6. `lightning_swap` proved the swap across two chains, but
//! its Knots chain ran at level 0 — `Blake2bHeight` at `INT_MAX`, 80-byte
//! v1 headers, behaviourally Bitcoin Core. Both legs were the same kind of
//! chain wearing different labels.
//!
//! Here the Knots chain has activated BLAKE2b before either channel is
//! funded, so the XBT leg lives entirely under 164-byte v2 headers while
//! the BTC leg stays on v1. The two sides genuinely differ, and the
//! scenario asserts that before asserting anything about the swap —
//! otherwise it could pass by being `lightning_swap` again.
//!
//! Note which binary is which. The XBT nodes are built with the `blake2b`
//! feature; the BTC nodes are stock `dln-node`, which has never heard of a
//! v2 header. One side of this swap does not know the other's chain
//! changed, and does not need to.
//!
//! Happy path only, as in 02.4: neither party abandons and no CLTV
//! ordering is asserted. Mission 04 is where that gets tested.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tracing::info;

use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::dln_node_client::{DlnNode, SignerMode};
use dln_e2e_harness::{relay, util};

/// Height at which the Knots chain activates BLAKE2b. Both channels are
/// funded well above it, so no block either leg depends on is v1.
const ACTIVATION_HEIGHT: u64 = 115;

/// Mined on the Knots chain before anything else happens — past both
/// coinbase maturity and the activation height.
const KNOTS_PREMINE: u64 = 130;

const CHANNEL_SATS: u64 = 2_000_000;
const PUSH_MSAT: u64 = 500_000;

/// Distinct per chain, so a balance assertion cannot pass by coincidence.
const CORE_LEG_MSAT: u64 = 120_000;
const KNOTS_LEG_MSAT: u64 = 250_000;

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
    let output_dir = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("lightning-swap")),
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir)?;
    info!("Output directory: {}", output_dir.display());

    // The XBT nodes must be able to read a v2 header. The BTC nodes must
    // not need to — they are left as stock dln-node, built by the harness.
    // Overridable so the negative control can be run: with
    // KNOTS_FEATURES set to anything else, the XBT nodes cannot read
    // their own chain and this scenario must fail.
    if std::env::var("KNOTS_FEATURES").is_err() {
        std::env::set_var("KNOTS_FEATURES", "blake2b");
    }
    let knots_binary = util::build_knots_node()?;
    anyhow::ensure!(
        std::env::var("DLN_NODE_BINARY").is_err(),
        "DLN_NODE_BINARY is set, which would put the same binary on both \
         chains and defeat the point of this scenario"
    );

    // ── Step 1: two chains that differ ──────────────────────────────────
    info!("Step 1: Core at level 0, Knots activating BLAKE2b at {ACTIVATION_HEIGHT}");
    let core = BitcoindHarness::start().await;
    let knots = BitcoindHarness::start_knots_activating_at(ACTIVATION_HEIGHT).await;
    let core_miner = core.get_new_address().await;
    let knots_miner = knots.get_new_address().await;
    core.mine_blocks(110, &core_miner).await;
    knots.mine_blocks(KNOTS_PREMINE, &knots_miner).await;

    // ── Step 2: the chains really are different ─────────────────────────
    // Without this the scenario could pass at level 0 and prove nothing.
    info!("Step 2: Verifying the two chains use different header formats");
    assert_header(&core, 110, 1).await.context("the BTC chain should be v1")?;
    assert_header(&knots, ACTIVATION_HEIGHT - 1, 1).await?;
    assert_header(&knots, ACTIVATION_HEIGHT, 2).await?;
    assert_header(&knots, KNOTS_PREMINE, 2).await?;
    info!("  BTC chain v1 at 110; XBT chain v1 at {}, v2 at {ACTIVATION_HEIGHT} and {KNOTS_PREMINE}",
          ACTIVATION_HEIGHT - 1);

    info!("Step 3: Four nodes — XBT side reads v2 headers, BTC side is stock");
    let (_relay_container, relay_url) = relay::start_relay().await;

    let alice_core = DlnNode::start_on(
        "alice-core", SignerMode::Plain, None,
        &core, &core_miner, &relay_url, &output_dir,
    ).await.context("alice-core failed to start")?;
    let bob_core = DlnNode::start_on(
        "bob-core", SignerMode::Plain, None,
        &core, &core_miner, &relay_url, &output_dir,
    ).await.context("bob-core failed to start")?;
    let alice_knots = DlnNode::start_on(
        "alice-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_miner, &relay_url, &output_dir,
    ).await.context("alice-knots failed to start")?;
    let bob_knots = DlnNode::start_on(
        "bob-knots", SignerMode::Plain, Some(&knots_binary),
        &knots, &knots_miner, &relay_url, &output_dir,
    ).await.context("bob-knots failed to start")?;

    // Bob pays on Core, Alice pays on Knots, so each funds their own leg.
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

    wait_for(Duration::from_secs(180), "both channels ready", || async {
        core.mine_blocks(1, &core_miner).await;
        knots.mine_blocks(1, &knots_miner).await;
        Ok(bob_core.has_ready_channel_with(&alice_core_id).await
            && alice_knots.has_ready_channel_with(&bob_knots_id).await)
    })
    .await?;
    info!("  both channels ready");

    // ── Step 4: the XBT channel is funded under v2 headers ──────────────
    // A channel that opened before activation would be 03.4's case. This
    // one has to have started life on the BLAKE2b chain.
    info!("Step 4: Checking the XBT channel's funding block");
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
    anyhow::ensure!(
        funding_height >= ACTIVATION_HEIGHT,
        "the XBT channel was funded in block {funding_height}, below activation at \
         {ACTIVATION_HEIGHT} — this leg did not open on a BLAKE2b chain"
    );
    assert_header(&knots, funding_height, 2).await?;
    info!("  XBT funding {funding_txid} confirmed in block {funding_height}, a v2 block");

    // ── Step 5: one hash, two invoices ──────────────────────────────────
    info!("Step 5: bob generates the secret; both legs use its hash");
    let secret_bytes = Keys::generate().secret_key().to_secret_bytes();
    let secret = hex::encode(secret_bytes);
    let payment_hash = hex::encode(Sha256::digest(secret_bytes));

    let alice_before = alice_core.get_balance_msat().await?;
    let bob_before = bob_knots.get_balance_msat().await?;

    let alice_invoice = alice_core
        .make_hold_invoice(CORE_LEG_MSAT, &payment_hash, "swap: alice receives core")
        .await
        .context("alice-core make_hold_invoice failed")?;
    let bob_invoice = bob_knots
        .make_hold_invoice(KNOTS_LEG_MSAT, &payment_hash, "swap: bob receives knots")
        .await
        .context("bob-knots make_hold_invoice failed")?;

    anyhow::ensure!(
        alice_invoice != bob_invoice,
        "both legs produced the same invoice"
    );
    for (who, inv) in [("alice", &alice_invoice), ("bob", &bob_invoice)] {
        anyhow::ensure!(
            invoice_payment_hash(inv)? == payment_hash,
            "{who}'s invoice does not carry the agreed payment hash"
        );
    }
    info!("  both invoices carry {payment_hash}");

    // ── Steps 6-8: fund both legs, then settle in order ─────────────────
    // Everything must overlap. Neither payment can complete until its payee
    // settles, and bob must not settle until alice's leg is also funded —
    // otherwise he takes his side while she has nothing held.
    info!("Step 6: funding both legs");
    let started = Instant::now();

    // Bob's Core payment cannot complete until alice settles the Core leg,
    // and she cannot do that until she has seen the preimage — which only
    // appears when bob settles the Knots leg. So alice's settle must run
    // inside this concurrent block, not after it: awaiting bob's payment
    // first would deadlock.
    let bob_pays_core = bob_core.pay_invoice(&alice_invoice);

    let alice_flow = async {
        // Completes when bob settles, returning the preimage he revealed.
        let observed = alice_knots
            .pay_invoice(&bob_invoice)
            .await
            .context("alice-knots pay_invoice failed")?;
        anyhow::ensure!(
            observed == secret,
            "alice observed {observed}, expected {secret}"
        );
        info!(
            "Step 8: alice observed the preimage from her own payment after {:.1}s",
            started.elapsed().as_secs_f32()
        );

        // Settle with the observed value, never the test's own copy. This is
        // what releases bob's Core payment.
        alice_core
            .settle_hold_invoice(&observed)
            .await
            .context("alice-core settle_hold_invoice failed")?;
        Ok::<String, anyhow::Error>(observed)
    };

    let bob_settles = async {
        wait_for(Duration::from_secs(120), "both legs held", || async {
            Ok(alice_core.lookup_invoice(&payment_hash).await.is_ok()
                && bob_knots.lookup_invoice(&payment_hash).await.is_ok())
        })
        .await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        info!("Step 7: bob settles the knots leg, revealing the secret");
        bob_knots.settle_hold_invoice(&secret).await
    };

    let (core_payment, alice_result, settled) =
        tokio::join!(bob_pays_core, alice_flow, bob_settles);
    settled.context("bob-knots settle_hold_invoice failed")?;
    let observed = alice_result?;
    let core_preimage = core_payment.context("bob-core pay_invoice failed")?;
    anyhow::ensure!(
        core_preimage == observed,
        "the core leg settled with a different preimage"
    );

    // ── Step 9: both parties ended up with what they wanted ─────────────
    info!("Step 9: verifying balances on both chains");
    wait_for(Duration::from_secs(60), "balances to settle", || async {
        Ok(alice_core.get_balance_msat().await? >= alice_before + CORE_LEG_MSAT
            && bob_knots.get_balance_msat().await? >= bob_before + KNOTS_LEG_MSAT)
    })
    .await?;

    let alice_after = alice_core.get_balance_msat().await?;
    let bob_after = bob_knots.get_balance_msat().await?;
    info!("  alice on core:  {alice_before} -> {alice_after} msat");
    info!("  bob on knots:   {bob_before} -> {bob_after} msat");

    Ok(())
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

/// Extract the payment hash from a BOLT11 invoice's tagged fields.
fn invoice_payment_hash(invoice: &str) -> Result<String> {
    use lightning_invoice::Bolt11Invoice;
    use std::str::FromStr;
    let parsed = Bolt11Invoice::from_str(invoice)
        .map_err(|e| anyhow::anyhow!("invoice did not parse: {e:?}"))?;
    Ok(parsed.payment_hash().to_string())
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
