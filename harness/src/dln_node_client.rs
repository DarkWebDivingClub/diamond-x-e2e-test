use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip44;
use nwc::nostr::nips::nip47::{
    CancelHoldInvoiceRequest, LookupInvoiceRequest, MakeHoldInvoiceRequest, MakeInvoiceRequest,
    Method, NostrWalletConnectUri, PayInvoiceRequest, PayOnchainRequest, Request, RequestParams,
    Response, ResponseResult, SettleHoldInvoiceRequest,
};
use serde_json::{json, Value};

use crate::bitcoind::BitcoindHarness;
use crate::process::ManagedChild;
use crate::util;

/// NCC control request kind (must match dln-node's CONTROL_REQUEST_KIND).
const CONTROL_REQUEST_KIND: u16 = 23198;
/// NCC control response kind (must match dln-node's CONTROL_RESPONSE_KIND).
const CONTROL_RESPONSE_KIND: u16 = 23199;

// ── Config structs for config.toml serialization ─────────────────────────

#[derive(serde::Serialize)]
struct LdkConfig {
    node: LdkNodeConfig,
    nostr: LdkNostrConfig,
    wallet: LdkWalletConfig,
    bitcoind: LdkBitcoindConfig,
    signer: LdkSignerConfig,
}

#[derive(serde::Serialize)]
struct LdkNodeConfig {
    network: String,
    listening_port: u16,
    data_dir: String,
}

#[derive(serde::Serialize)]
struct LdkNostrConfig {
    relay: String,
    private_key: String,
    /// Whose grants the node accepts.
    ///
    /// **Required since mission 25.3.** An empty list accepts none, so a
    /// harness that omits it starts a node that correctly refuses
    /// everything. The node used to keep an owners list nothing ever wrote
    /// to and apply every grant it saw — dln-node#1.
    owners: Vec<String>,
}

#[derive(serde::Serialize)]
struct LdkWalletConfig {
    max_channel_size_sats: u64,
    min_channel_size_sats: u64,
    auto_accept_channels: bool,
}

#[derive(serde::Serialize)]
struct LdkBitcoindConfig {
    rpc_host: String,
    rpc_port: u16,
    rpc_user: String,
    rpc_password: String,
}

#[derive(serde::Serialize)]
struct LdkSignerConfig {
    transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nsec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signer_pubkey: Option<String>,
}

/// How the node signs.
///
/// `Plain` bypasses VLS entirely: ldk-node uses its own KeysManager and BDK
/// wallet, seeded from its storage dir. No policy validation. `Embedded` runs
/// an in-process VLS signer with a NullTransport. `Nostr` spawns a separate
/// signer process reached over a relay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignerMode {
    Plain,
    Embedded,
    Nostr,
}

// ── Public API ───────────────────────────────────────────────────────────

/// Blackbox dln-node node managed as a child process.
///
/// All test operations go through the Nostr interface — NCC for control
/// commands (open/close/list channels) and NWC for wallet commands
/// (pay_invoice, get_balance, get_info).
pub struct DlnNode {
    pub child: ManagedChild,
    pub signer_child: Option<ManagedChild>,
    pub ln_port: u16,
    node_id: String,
    ncc_client: Client,
    ncc_secret: SecretKey,
    nwc_client: Client,
    nwc_uri: NostrWalletConnectUri,
    service_pubkey: PublicKey,
}

impl DlnNode {
    /// Start dln-node as a child process, publish Nostr grants,
    /// wait for readiness, and fund the on-chain wallet.
    /// Start with a remote signer over Nostr, labelled "alice".
    pub async fn start(
        bitcoind: &BitcoindHarness,
        miner_address: &str,
        relay_url: &str,
        output_dir: &Path,
    ) -> Result<Self> {
        Self::start_named("alice", SignerMode::Nostr, bitcoind, miner_address, relay_url, output_dir).await
    }

    /// Start a named node with the given signer mode, using the default
    /// node binary.
    ///
    /// `name` labels the child processes and log files, so several nodes can
    /// run side by side in one scenario.
    pub async fn start_named(
        name: &str,
        signer_mode: SignerMode,
        bitcoind: &BitcoindHarness,
        miner_address: &str,
        relay_url: &str,
        output_dir: &Path,
    ) -> Result<Self> {
        Self::start_on(name, signer_mode, None, bitcoind, miner_address, relay_url, output_dir)
            .await
    }

    /// Start a named node from a specific binary.
    ///
    /// A scenario spanning two chains needs a different binary per chain —
    /// `dln-node` on one side and `dln-node-knots` on the other — so the
    /// path cannot be a process-wide default.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_on(
        name: &str,
        signer_mode: SignerMode,
        node_binary: Option<&str>,
        bitcoind: &BitcoindHarness,
        miner_address: &str,
        relay_url: &str,
        output_dir: &Path,
    ) -> Result<Self> {
        let ln_port = util::free_port();

        // ── 1. Generate keys ────────────────────────────────────────────
        let service_keys = Keys::generate();
        let service_pubkey = service_keys.public_key();

        let controller_keys = Keys::generate();
        let controller_secret = controller_keys.secret_key().clone();
        let controller_pubkey = controller_keys.public_key();

        let nwc_keys = Keys::generate();
        let nwc_secret = nwc_keys.secret_key().clone();
        let nwc_pubkey = nwc_keys.public_key();

        let owner_keys = Keys::generate();

        // ── 2. Publish grants to relay BEFORE spawning ──────────────────
        let grant_client = Client::builder().signer(owner_keys.clone()).build();
        grant_client.add_relay(relay_url).await?;
        grant_client.connect().await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        // NCC grant: control methods for the controller key
        let ncc_grant = json!({
            "control": {
                "open_channel": {},
                "list_channels": {},
                "close_channel": {},
            }
        });
        let d_ncc = format!("{service_pubkey}:{controller_pubkey}");
        let ncc_event = EventBuilder::new(Kind::Custom(nostr_ln::GRANT_KIND), ncc_grant.to_string())
            .tag(Tag::parse(["d", &d_ncc]).expect("d tag"))
            .tag(Tag::public_key(service_pubkey));
        grant_client.send_event_builder(ncc_event).await?;

        // NWC grant: wallet methods for the NWC client key
        let nwc_grant = json!({
            "methods": {
                "get_info": {},
                "pay_invoice": {},
                "get_balance": {},
                "make_invoice": {},
                "pay_onchain": {},
                "make_hold_invoice": {},
                "settle_hold_invoice": {},
                "cancel_hold_invoice": {},
                "lookup_invoice": {},
                "make_new_address": {},
            }
        });
        let d_nwc = format!("{service_pubkey}:{nwc_pubkey}");
        let nwc_event = EventBuilder::new(Kind::Custom(nostr_ln::GRANT_KIND), nwc_grant.to_string())
            .tag(Tag::parse(["d", &d_nwc]).expect("d tag"))
            .tag(Tag::public_key(service_pubkey));
        grant_client.send_event_builder(nwc_event).await?;

        tracing::info!("published NCC + NWC grants to relay");

        // ── 3. Signer ───────────────────────────────────────────────────
        // Embedded: the node runs an in-process VLS signer, nothing to spawn.
        // Nostr: spawn a separate eln-signer that the node reaches by relay.
        let np_keys = Keys::generate();
        let signer_keys = Keys::generate();
        let np_pubkey_hex = np_keys.public_key().to_hex();
        let np_nsec_hex = np_keys.secret_key().to_secret_hex();
        let signer_pubkey_hex = signer_keys.public_key().to_hex();
        let signer_nsec_hex = signer_keys.secret_key().to_secret_hex();

        let signer_child = if signer_mode == SignerMode::Nostr {
            tracing::info!("{name} nostr NP pubkey: {}", np_pubkey_hex);
            tracing::info!("{name} nostr signer pubkey: {}", signer_pubkey_hex);

            let signer_binary = eln_signer_binary()?;
            let signer_args: Vec<&str> = vec![
                "--relay", relay_url,
                "--proxy-pubkey", &np_pubkey_hex,
                "--nsec", &signer_nsec_hex,
                "--network", bitcoind.network(),
                "--integration-test",
                "--protocol-version", "6",
                "--policy-filter", "policy-commitment-htlc-routing-balance:warn",
                "--policy-filter", "policy-routing-balanced:warn",
            ];
            let child = ManagedChild::spawn(
                &format!("{name}-eln-signer"),
                &signer_binary,
                &signer_args,
                output_dir,
            )?;

            // Give signer time to connect to relay and subscribe
            tokio::time::sleep(Duration::from_secs(2)).await;
            Some(child)
        } else {
            match signer_mode {
                SignerMode::Plain => tracing::info!("{name} using plain LDK keys (no VLS)"),
                _ => tracing::info!("{name} using embedded (in-process) VLS signer"),
            }
            None
        };

        // ── 4. Write config.toml ────────────────────────────────────────
        let cwd = PathBuf::from(util::unique_tmp_dir("ldk-ctrl"));
        std::fs::create_dir_all(&cwd)?;
        let data_dir = cwd.join("data");
        std::fs::create_dir_all(&data_dir)?;

        let config = LdkConfig {
            node: LdkNodeConfig {
                network: bitcoind.network().to_string(),
                listening_port: ln_port,
                data_dir: data_dir.to_string_lossy().to_string(),
            },
            nostr: LdkNostrConfig {
                relay: relay_url.to_string(),
                private_key: service_keys.secret_key().to_secret_hex(),
                // The key whose grants this node accepts — the same one
                // that signed the two grants above.
                owners: vec![owner_keys.public_key().to_hex()],
            },
            wallet: LdkWalletConfig {
                max_channel_size_sats: 10_000_000,
                min_channel_size_sats: 20_000,
                auto_accept_channels: true,
            },
            bitcoind: LdkBitcoindConfig {
                rpc_host: bitcoind.rpc_host().to_string(),
                rpc_port: bitcoind.rpc_port(),
                rpc_user: bitcoind.rpc_user().to_string(),
                rpc_password: bitcoind.rpc_password().to_string(),
            },
            signer: match signer_mode {
                SignerMode::Nostr => LdkSignerConfig {
                    transport: "nostr".to_string(),
                    relay: Some(relay_url.to_string()),
                    nsec: Some(np_nsec_hex),
                    signer_pubkey: Some(signer_pubkey_hex),
                },
                SignerMode::Embedded => LdkSignerConfig {
                    transport: "embedded".to_string(),
                    relay: None,
                    nsec: None,
                    signer_pubkey: None,
                },
                SignerMode::Plain => LdkSignerConfig {
                    transport: "none".to_string(),
                    relay: None,
                    nsec: None,
                    signer_pubkey: None,
                },
            },
        };

        let config_toml = toml::to_string(&config).context("serialize config.toml")?;
        std::fs::write(cwd.join("config.toml"), &config_toml)?;
        tracing::info!("wrote config.toml to {}", cwd.display());

        // ── 5. Spawn dln-node binary ──────────────────────────────
        let binary = match node_binary {
            Some(path) => path.to_string(),
            None => dln_node_binary()?,
        };
        let child = ManagedChild::spawn_with_cwd(
            &format!("{name}-dln-node"),
            &binary,
            &[],
            output_dir,
            Some(&cwd),
        )?;

        // ── 6. Set up NWC client and poll get_info ──────────────────────
        let relay = RelayUrl::parse(relay_url)?;
        let nwc_uri = NostrWalletConnectUri::new(
            service_pubkey,
            vec![relay],
            nwc_secret.clone(),
            None,
        );

        let nwc_client = Client::builder().signer(nwc_keys).build();
        nwc_client.add_relay(relay_url).await?;
        nwc_client.connect().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        nwc_client
            .subscribe(
                Filter::new()
                    .kind(Kind::WalletConnectResponse)
                    .author(service_pubkey),
            )
            .await?;

        tracing::info!("polling get_info to confirm dln-node is ready...");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let node_id = loop {
            match Self::send_nwc_request_static(
                &nwc_client,
                &nwc_uri,
                service_pubkey,
                Request::get_info(),
                None,
            )
            .await
            {
                Ok(resp) => {
                    if let Ok(info) = resp.to_get_info() {
                        if let Some(pubkey) = info.pubkey {
                            break pubkey;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("get_info poll: {e:#}");
                }
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("timeout waiting for dln-node get_info (60s)");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        tracing::info!("dln-node ready, node_id={node_id}");

        // ── 7. Fund on-chain wallet via NWC make_new_address ────────────
        let addr_resp = Self::send_nwc_request_static(
            &nwc_client,
            &nwc_uri,
            service_pubkey,
            Request::make_new_address(),
            None,
        )
        .await
        .context("NWC make_new_address failed")?;
        let addr = addr_resp
            .to_make_new_address()
            .context("make_new_address response parse failed")?
            .address;

        let funding_txid = bitcoind.send_to_address(&addr, 0.05).await;
        if bitcoind.can_mine() {
            bitcoind.mine_blocks(1, miner_address).await;
        } else {
            // A shared chain confirms on its own schedule. Waiting is the
            // only option, and it has to be a hard wait: carrying on with an
            // unfunded wallet would fail later and further from the cause.
            tracing::info!("{name}: no mining authority, waiting for funding to confirm");
            bitcoind
                .wait_for_confirmations(&funding_txid, 1, Duration::from_secs(600))
                .await
                .map_err(|e| anyhow::anyhow!("{name} funding never confirmed: {e}"))?;
        }

        // A chain we do not control needs a deadline measured in blocks, not
        // in the seconds a regtest chain takes to mine on demand.
        let balance_timeout = if bitcoind.can_mine() { 30 } else { 600 };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(balance_timeout);
        loop {
            match Self::send_nwc_request_static(
                &nwc_client,
                &nwc_uri,
                service_pubkey,
                Request::get_balance(),
                None,
            )
            .await
            {
                Ok(resp) => {
                    if let Ok(balance) = resp.to_get_balance() {
                        if balance.balance >= 4_000_000_000 {
                            break;
                        }
                    }
                }
                Err(_) => {}
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("dln-node funding timeout");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // ── 8. Set up NCC client ────────────────────────────────────────
        let ncc_client = Client::builder().signer(controller_keys).build();
        ncc_client.add_relay(relay_url).await?;
        ncc_client.connect().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        ncc_client
            .subscribe(
                Filter::new()
                    .kind(Kind::Custom(CONTROL_RESPONSE_KIND))
                    .author(service_pubkey),
            )
            .await?;

        tracing::info!(
            "dln-node started, node_id={node_id}, ln_port={ln_port}",
        );

        Ok(Self {
            child,
            signer_child,
            ln_port,
            node_id,
            ncc_client,
            ncc_secret: controller_secret,
            nwc_client,
            nwc_uri,
            service_pubkey,
        })
    }

    // ── Public API (all via Nostr) ─────────────────────────────────────

    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    /// Open a channel via NCC. `push_amount` is in **sats** (the NCC handler
    /// converts to msat internally).
    pub async fn open_channel(
        &self,
        node_id: &str,
        addr: &str,
        amount: u64,
        push_amount: Option<u64>,
    ) -> Result<()> {
        let mut params = json!({
            "pubkey": node_id,
            "host": addr,
            "amount": amount,
        });
        if let Some(push) = push_amount {
            params["push_amount"] = json!(push);
        }
        let response = self
            .send_ncc_request(json!({
                "method": "open_channel",
                "params": params,
            }))
            .await
            .context("NCC open_channel failed")?;
        if !response["error"].is_null() {
            anyhow::bail!("open_channel error: {}", response["error"]);
        }
        Ok(())
    }

    /// Pay a Lightning invoice via NWC.
    /// Pay a BOLT11 invoice. Returns the preimage.
    ///
    /// Against a hold invoice this does not complete until the payee
    /// settles, so the returned preimage is how a payer learns a secret it
    /// did not choose — the mechanism a cross-chain swap depends on.
    pub async fn pay_invoice(&self, invoice: &str) -> Result<String> {
        let response = self
            .send_nwc_request_timeout(
                Request::pay_invoice(PayInvoiceRequest::new(invoice)),
                Some(Duration::from_secs(300)),
            )
            .await
            .context("NWC pay_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("pay_invoice error: {} [{:?}]", err.message, err.code);
        }
        let paid = response
            .to_pay_invoice()
            .context("pay_invoice response parse failed")?;
        Ok(paid.preimage)
    }

    /// Create a BOLT11 invoice via NWC.
    pub async fn make_invoice(&self, amount_msat: u64, description: &str) -> Result<String> {
        let response = self
            .send_nwc_request(Request::make_invoice(MakeInvoiceRequest {
                amount: amount_msat,
                description: Some(description.to_string()),
                description_hash: None,
                expiry: None,
            }))
            .await
            .context("NWC make_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("make_invoice error: {} [{:?}]", err.message, err.code);
        }
        let made = response
            .to_make_invoice()
            .context("make_invoice response parse failed")?;
        Ok(made.invoice)
    }

    /// Create a hold invoice for a payment hash chosen by the caller.
    ///
    /// Unlike `make_invoice`, the preimage is not generated by the payee —
    /// which is what lets two legs of a swap share one hash. The invoice is
    /// not settled by payment; the payee must call `settle_hold_invoice`.
    pub async fn make_hold_invoice(
        &self,
        amount_msat: u64,
        payment_hash: &str,
        description: &str,
    ) -> Result<String> {
        let response = self
            .send_nwc_request(Request {
                method: Method::MakeHoldInvoice,
                params: RequestParams::MakeHoldInvoice(MakeHoldInvoiceRequest {
                    amount: amount_msat,
                    description: Some(description.to_string()),
                    description_hash: None,
                    expiry: None,
                    payment_hash: payment_hash.to_string(),
                    min_cltv_expiry_delta: None,
                }),
            })
            .await
            .context("NWC make_hold_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("make_hold_invoice error: {} [{:?}]", err.message, err.code);
        }
        match response.result {
            Some(ResponseResult::MakeHoldInvoice(made)) => made
                .invoice
                .context("make_hold_invoice returned no invoice"),
            other => anyhow::bail!("unexpected make_hold_invoice result: {other:?}"),
        }
    }

    /// Settle a hold invoice by revealing its preimage.
    pub async fn settle_hold_invoice(&self, preimage: &str) -> Result<()> {
        let response = self
            .send_nwc_request(Request {
                method: Method::SettleHoldInvoice,
                params: RequestParams::SettleHoldInvoice(SettleHoldInvoiceRequest {
                    preimage: preimage.to_string(),
                }),
            })
            .await
            .context("NWC settle_hold_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("settle_hold_invoice error: {} [{:?}]", err.message, err.code);
        }
        Ok(())
    }

    /// Cancel a hold invoice, failing back any payment held against it.
    pub async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<()> {
        let response = self
            .send_nwc_request(Request {
                method: Method::CancelHoldInvoice,
                params: RequestParams::CancelHoldInvoice(CancelHoldInvoiceRequest {
                    payment_hash: payment_hash.to_string(),
                }),
            })
            .await
            .context("NWC cancel_hold_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("cancel_hold_invoice error: {} [{:?}]", err.message, err.code);
        }
        Ok(())
    }

    /// Look an invoice up by payment hash. Returns the raw result so callers
    /// can inspect whatever state fields the node reports.
    pub async fn lookup_invoice(&self, payment_hash: &str) -> Result<Value> {
        let response = self
            .send_nwc_request(Request::lookup_invoice(LookupInvoiceRequest {
                payment_hash: Some(payment_hash.to_string()),
                invoice: None,
            }))
            .await
            .context("NWC lookup_invoice failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("lookup_invoice error: {} [{:?}]", err.message, err.code);
        }
        Ok(serde_json::to_value(
            response.result.context("lookup_invoice returned no result")?,
        )?)
    }

    /// Get a fresh on-chain address from the node's BDK wallet.
    pub async fn new_onchain_address(&self) -> Result<String> {
        let response = self
            .send_nwc_request(Request::make_new_address())
            .await
            .context("NWC make_new_address failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("make_new_address error: {} [{:?}]", err.message, err.code);
        }
        let made = response
            .to_make_new_address()
            .context("make_new_address response parse failed")?;
        Ok(made.address)
    }

    /// Send an on-chain payment. Returns the txid.
    pub async fn pay_onchain(&self, address: &str, amount_sats: u64) -> Result<String> {
        let response = self
            .send_nwc_request(Request::pay_onchain(PayOnchainRequest {
                address: address.to_string(),
                amount: amount_sats,
                feerate: None,
            }))
            .await
            .context("NWC pay_onchain failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("pay_onchain error: {} [{:?}]", err.message, err.code);
        }
        let paid = response
            .to_pay_onchain()
            .context("pay_onchain response parse failed")?;
        Ok(paid.txid)
    }

    /// List channels via NCC. Returns the JSON array of channel objects.
    pub async fn list_channels(&self) -> Result<Vec<Value>> {
        let response = self
            .send_ncc_request(json!({
                "method": "list_channels",
                "params": {},
            }))
            .await
            .context("NCC list_channels failed")?;
        if !response["error"].is_null() {
            anyhow::bail!("list_channels error: {}", response["error"]);
        }
        let channels = response["result"]["channels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(channels)
    }

    /// Check if there is any channel with the given node_id (any state).
    pub async fn has_channel_with(&self, node_id: &str) -> bool {
        match self.list_channels().await {
            Ok(channels) => channels
                .iter()
                .any(|c| c["peer_pubkey"].as_str() == Some(node_id)),
            Err(_) => false,
        }
    }

    /// Check if there is an active (ready + usable) channel with the given node_id.
    pub async fn has_ready_channel_with(&self, node_id: &str) -> bool {
        match self.list_channels().await {
            Ok(channels) => channels.iter().any(|c| {
                c["peer_pubkey"].as_str() == Some(node_id)
                    && c["state"].as_str() == Some("active")
            }),
            Err(_) => false,
        }
    }

    /// Close a channel via NCC.
    pub async fn close_channel(&self, channel_id: &str, force: bool) -> Result<()> {
        let response = self
            .send_ncc_request(json!({
                "method": "close_channel",
                "params": {
                    "id": channel_id,
                    "force": force,
                },
            }))
            .await
            .context("NCC close_channel failed")?;
        if !response["error"].is_null() {
            anyhow::bail!("close_channel error: {}", response["error"]);
        }
        Ok(())
    }

    /// Get on-chain + lightning balance in millisatoshis via NWC.
    pub async fn get_balance_msat(&self) -> Result<u64> {
        let response = self
            .send_nwc_request(Request::get_balance())
            .await
            .context("NWC get_balance failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("get_balance error: {} [{:?}]", err.message, err.code);
        }
        let balance_response = response
            .to_get_balance()
            .context("get_balance response parse failed")?;
        Ok(balance_response.balance)
    }

    /// The node's view of the chain tip, over NWC `get_info`.
    ///
    /// This is `ldk-node`'s `current_best_block`, which it reaches through
    /// `lightning-block-sync`. If header parsing broke, this is where it
    /// would show: the height stops advancing while bitcoind's keeps going.
    pub async fn block_height(&self) -> Result<u32> {
        let response = self
            .send_nwc_request(Request::get_info())
            .await
            .context("NWC get_info failed")?;
        if let Some(err) = response.error {
            anyhow::bail!("get_info error: {} [{:?}]", err.message, err.code);
        }
        response
            .to_get_info()
            .context("get_info response parse failed")?
            .block_height
            .context("get_info returned no block_height")
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Send an NCC control request (NIP-04 encrypted, kind 23198) and read
    /// the response (kind 23199).
    async fn send_ncc_request(&self, payload: Value) -> Result<Value> {
        let encrypted =
            // NIP-44. NIP-XX has never permitted NIP-04 — dln-node accepted
            // it anyway, which is the other half of #3.
            nip44::encrypt(
                &self.ncc_secret,
                &self.service_pubkey,
                payload.to_string(),
                nip44::Version::V2,
            )?;
        let request_event = EventBuilder::new(Kind::Custom(CONTROL_REQUEST_KIND), encrypted)
            .tag(Tag::public_key(self.service_pubkey));
        self.ncc_client
            .send_event_builder(request_event)
            .await?;

        let response_event =
            Self::read_ncc_response(&self.ncc_client, self.service_pubkey).await?;
        let decrypted =
            nip44::decrypt(&self.ncc_secret, &self.service_pubkey, &response_event.content)?;
        let response: Value = serde_json::from_str(&decrypted)?;
        Ok(response)
    }

    /// Wait for a control response event (kind 23199) from the service.
    async fn read_ncc_response(client: &Client, service_pubkey: PublicKey) -> Result<Event> {
        let timeout = Duration::from_secs(10);
        let event = tokio::time::timeout(timeout, async {
            let mut notifications = client.notifications();
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    let event = event.as_ref();
                    if event.kind == Kind::Custom(CONTROL_RESPONSE_KIND)
                        && event.pubkey == service_pubkey
                    {
                        return Some(event.clone());
                    }
                }
            }
            None
        })
        .await
        .context("timeout waiting for NCC response")?
        .context("notification stream ended before NCC response")?;
        Ok(event)
    }

    /// Send an NWC request and read the response.
    async fn send_nwc_request(&self, request: Request) -> Result<Response> {
        self.send_nwc_request_timeout(request, None).await
    }

    async fn send_nwc_request_timeout(
        &self,
        request: Request,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        Self::send_nwc_request_static(
            &self.nwc_client,
            &self.nwc_uri,
            self.service_pubkey,
            request,
            timeout,
        )
        .await
    }

    /// Static version of send_nwc_request used during startup before `self` exists.
    /// `timeout` overrides the default 10s wait. Paying a hold invoice is
    /// long-running by design — the response cannot arrive until the payee
    /// settles — so it needs a longer one. Everything else should fail fast.
    async fn send_nwc_request_static(
        client: &Client,
        uri: &NostrWalletConnectUri,
        service_pubkey: PublicKey,
        request: Request,
        timeout: Option<Duration>,
    ) -> Result<Response> {
        // **NIP-44, not `to_event`.** The fork's `Request::to_event`
        // encrypts with NIP-04, and `dln-node` used to accept it — which is
        // [dln-node#3](https://github.com/DarkWebDivingClub/dln-node/issues/3).
        // This harness has been depending on that defect, in the same way
        // it was writing `access_rate`: a fixture agreeing with the bug is
        // why nothing caught it.
        let client_keys = Keys::new(uri.secret.clone());
        let ciphertext = nip44::encrypt(
            &uri.secret,
            &service_pubkey,
            request.as_json(),
            nip44::Version::V2,
        )
        .context("nip44 encrypt")?;
        let request_event = EventBuilder::new(Kind::WalletConnectRequest, ciphertext)
            .tag(Tag::public_key(service_pubkey))
            .sign_with_keys(&client_keys)
            .context("sign NWC request")?;
        client.send_event(&request_event).await?;

        let timeout = timeout.unwrap_or(Duration::from_secs(10));
        let uri_clone = uri.clone();
        let response = tokio::time::timeout(timeout, async {
            let mut notifications = client.notifications();
            while let Some(notification) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = notification {
                    let event = event.as_ref();
                    if event.kind == Kind::WalletConnectResponse
                        && event.pubkey == service_pubkey
                    {
                        let plaintext = nip44::decrypt(
                            &uri_clone.secret,
                            &event.pubkey,
                            &event.content,
                        )
                        .context("nip44 decrypt")?;
                        let resp: Response = serde_json::from_str(&plaintext)
                            .context("failed to decode NWC response")?;
                        return Ok(resp);
                    }
                }
            }
            anyhow::bail!("notification stream ended before NWC response")
        })
        .await
        .context("timeout waiting for NWC response")??;
        Ok(response)
    }
}

/// Determine the eln-signer binary path.
fn eln_signer_binary() -> Result<String> {
    if let Ok(path) = std::env::var("ELN_SIGNER_BINARY") {
        tracing::info!("Using pre-built eln-signer binary: {path}");
        return Ok(path);
    }

    let transport_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("system-test parent")
        .join("eln-signer");

    tracing::info!("Building eln-signer from {}", transport_dir.display());
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "eln-signer"])
        .current_dir(&transport_dir)
        .status()
        .context("failed to run cargo build for eln-signer")?;

    anyhow::ensure!(status.success(), "cargo build eln-signer failed with {status}");

    let binary = transport_dir.join("target/debug/eln-signer");
    anyhow::ensure!(binary.exists(), "eln-signer binary not found at {}", binary.display());
    Ok(binary.to_string_lossy().to_string())
}

/// Determine the dln-node binary path: `DLN_NODE_BINARY` env var,
/// or build from `/home/rene/git/dln-node` via `cargo build`.
fn dln_node_binary() -> Result<String> {
    if let Ok(path) = std::env::var("DLN_NODE_BINARY") {
        tracing::info!("Using pre-built dln-node binary: {path}");
        return Ok(path);
    }

    let dln_node_dir = PathBuf::from("/home/rene/git/dln-node");
    tracing::info!(
        "Building dln-node from {}",
        dln_node_dir.display()
    );
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "dln-node"])
        .current_dir(&dln_node_dir)
        .status()
        .context("failed to run cargo build for dln-node")?;

    anyhow::ensure!(
        status.success(),
        "cargo build dln-node failed with {status}"
    );

    let binary = dln_node_dir.join("target/debug/dln-node");
    anyhow::ensure!(
        binary.exists(),
        "dln-node binary not found at {}",
        binary.display()
    );
    Ok(binary.to_string_lossy().to_string())
}
