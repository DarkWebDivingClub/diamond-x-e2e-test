use serde_json::{json, Value};
use std::time::Duration;
use testcontainers::{
    core::IntoContainerPort,
    runners::AsyncRunner,
    ContainerAsync, GenericImage,
};
use testcontainers::ImageExt;

const RPC_USER: &str = "rpcuser";
const RPC_PASS: &str = "rpcpass";
const RPC_PORT: u16 = 18443;

pub struct BitcoindHarness {
    /// `None` when attached to a node we did not start — see `attach`.
    /// Dropping the harness must not stop somebody else's node.
    _container: Option<ContainerAsync<GenericImage>>,
    rpc_url: String,
    rpc_port: u16,
    rpc_user: String,
    rpc_password: String,
    wallet: String,
    network: String,
    /// Human name for messages. Both signets report network "signet", so
    /// without this a timeout cannot say which chain stalled.
    label: String,
    client: reqwest::Client,
}

/// How to reach a node this harness did not start.
///
/// A scenario running against a shared chain has no mining authority and
/// cannot choose the chain's parameters, so everything here is discovered
/// rather than decided.
#[derive(Clone, Debug)]
pub struct AttachConfig {
    pub rpc_host: String,
    pub rpc_port: u16,
    pub rpc_user: String,
    pub rpc_password: String,
    pub wallet: String,
    /// `signet`, `regtest`, ... — passed through to dln-node.
    pub network: String,
    /// What to call this chain in errors, e.g. `xbt.signet`. Both signets
    /// share a network name, so this is what makes a timeout actionable.
    pub label: String,
}

/// Bitcoin Core regtest image.
const CORE_IMAGE: (&str, &str) = ("ruimarinho/bitcoin-core", "latest");

/// Bitcoin Knots regtest image, built by `docker/knots/build.sh`.
const KNOTS_IMAGE: (&str, &str) = ("dwdc/bitcoin-knots", "29");

/// Coinbase headline for an activating regtest chain. Arbitrary, but every
/// party has to agree on it — it is consensus-critical on a real chain.
const ACTIVATION_HEADLINE: &str = "dwdc regtest blake2b activation";

impl BitcoindHarness {
    /// Start a Bitcoin Core regtest node.
    pub async fn start() -> Self {
        Self::start_with(CORE_IMAGE.0, CORE_IMAGE.1, &[]).await
    }

    /// Start a Bitcoin Knots regtest node at level 0 — no
    /// `-testactivationheight`, so `Blake2bHeight` stays at `INT_MAX` and
    /// the chain produces only v1 headers (80 bytes, SHA256d).
    ///
    /// `-blake2b_headline` is mandatory: the node refuses to start without
    /// it. Its value is consensus-critical on a real chain but arbitrary on
    /// a regtest chain that never activates.
    pub async fn start_knots() -> Self {
        Self::start_with(
            KNOTS_IMAGE.0,
            KNOTS_IMAGE.1,
            &["-blake2b_headline=dwdc regtest level 0"],
        )
        .await
    }

    /// Start a Bitcoin Knots regtest node that activates BLAKE2b at
    /// `height`, so blocks below it carry v1 headers and blocks at and
    /// above it carry v2 headers.
    ///
    /// The headline is not decoration here. Consensus requires the coinbase
    /// of the activation block to contain it, so a node started with a
    /// different one rejects the chain this one builds. The internal miner
    /// puts it there, which is why `generatetoaddress` is enough.
    pub async fn start_knots_activating_at(height: u64) -> Self {
        Self::start_with(
            KNOTS_IMAGE.0,
            KNOTS_IMAGE.1,
            &[
                &format!("-testactivationheight=blake2b@{height}"),
                &format!("-blake2b_headline={ACTIVATION_HEADLINE}"),
                // Scheduling an activation height makes the wallet sign with
                // SIGHASH_UNIFIED immediately, but a block below that height
                // may not carry such a signature — and the block assembler
                // throws rather than leaving the transaction out, so the node
                // cannot mine at all. See
                // doc/knots-unified-sighash-preactivation.md in the workspace.
                //
                // Only needed here: at level 0 nothing is scheduled, so the
                // wallet signs the legacy message anyway.
                "-walletoldsigs=1",
            ],
        )
        .await
    }

    /// Start whichever backend `BITCOIND_IMPL` selects — `core` (default)
    /// or `knots`.
    ///
    /// Lets one scenario run against either chain without duplicating it.
    /// Pair with `DLN_NODE_BINARY` to use the matching node build.
    pub async fn start_from_env() -> Self {
        match std::env::var("BITCOIND_IMPL").unwrap_or_else(|_| "core".to_string()).as_str() {
            "knots" => {
                tracing::info!("BITCOIND_IMPL=knots — using the Bitcoin Knots backend");
                Self::start_knots().await
            }
            "core" => Self::start().await,
            other => panic!("unknown BITCOIND_IMPL {other:?}, expected core or knots"),
        }
    }

    /// Start a regtest node from an arbitrary image, with extra daemon args
    /// appended to the standard set.
    pub async fn start_with(image: &str, tag: &str, extra_args: &[&str]) -> Self {
        let mut cmd = vec![
            "-regtest=1".to_string(),
            "-server=1".to_string(),
            "-txindex=1".to_string(),
            "-printtoconsole".to_string(),
            "-fallbackfee=0.0002".to_string(),
            "-rpcbind=0.0.0.0".to_string(),
            "-rpcallowip=0.0.0.0/0".to_string(),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASS}"),
        ];
        cmd.extend(extra_args.iter().map(|a| a.to_string()));

        let container = GenericImage::new(image, tag)
            .with_exposed_port(RPC_PORT.tcp())
            .with_cmd(cmd)
            .start()
            .await
            .unwrap_or_else(|e| panic!("Failed to start {image}:{tag} container: {e}"));

        let host_port = container
            .get_host_port_ipv4(RPC_PORT)
            .await
            .expect("Failed to get mapped bitcoind RPC port");

        let rpc_url = format!("http://localhost:{host_port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let harness = Self {
            _container: Some(container),
            rpc_url,
            rpc_port: host_port,
            rpc_user: RPC_USER.to_string(),
            rpc_password: RPC_PASS.to_string(),
            wallet: "testwallet".to_string(),
            network: "regtest".to_string(),
            label: format!("{image}:{tag}"),
            client,
        };

        harness.wait_until_ready().await;
        harness.create_wallet("testwallet").await;

        harness
    }

    /// Attach to a node that is already running and that we do not own.
    ///
    /// The chain is shared, so this harness cannot mine on it: use
    /// `wait_for_confirmations` instead of `mine_blocks`. Dropping it leaves
    /// the node running.
    pub async fn attach(cfg: AttachConfig) -> Self {
        let rpc_url = format!("http://{}:{}", cfg.rpc_host, cfg.rpc_port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        let harness = Self {
            _container: None,
            rpc_url,
            rpc_port: cfg.rpc_port,
            rpc_user: cfg.rpc_user,
            rpc_password: cfg.rpc_password,
            wallet: cfg.wallet,
            network: cfg.network,
            label: cfg.label,
            client,
        };

        harness.wait_until_ready().await;
        harness
    }

    /// The chain's network name, for anything that has to be told which
    /// chain it is on.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// False when attached to a chain we do not control. A scenario that
    /// mines unconditionally is a scenario that cannot run against a real
    /// chain, so this is worth asserting rather than discovering.
    pub fn can_mine(&self) -> bool {
        self._container.is_some()
    }

    /// Wait for `txid` to reach `want` confirmations.
    ///
    /// This is what replaces `mine_blocks` on a chain we do not control.
    /// It must fail loudly: a scenario that carries on with an unconfirmed
    /// channel proves nothing, so the timeout is an error rather than a
    /// shrug.
    pub async fn wait_for_confirmations(
        &self,
        txid: &str,
        want: u64,
        timeout: Duration,
    ) -> Result<u64, String> {
        let started = std::time::Instant::now();
        let mut last_seen: i64 = -1;

        while started.elapsed() < timeout {
            match self
                .rpc_call("gettransaction", json!([txid]))
                .await
                .ok()
                .and_then(|v| v.get("confirmations").and_then(|c| c.as_i64()))
            {
                Some(c) if c >= want as i64 => return Ok(c as u64),
                Some(c) => {
                    if c != last_seen {
                        tracing::info!(
                            "  {} on {}: {c}/{want} confirmations",
                            &txid[..12.min(txid.len())],
                            self.label
                        );
                        last_seen = c;
                    }
                }
                None => {}
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        Err(format!(
            "{txid} did not reach {want} confirmations on {} within {:?} \
             (last seen {last_seen}) — is {} producing blocks?",
            self.label, timeout, self.label
        ))
    }

    pub async fn create_wallet(&self, wallet: &str) {
        let _ = self
            .rpc_call("createwallet", json!([wallet]))
            .await
            .expect("createwallet RPC should succeed");
    }

    pub async fn get_new_address(&self) -> String {
        self.rpc_call("getnewaddress", json!([]))
            .await
            .expect("getnewaddress RPC should succeed")
            .as_str()
            .expect("getnewaddress result should be a string")
            .to_string()
    }

    pub async fn mine_blocks(&self, blocks: u64, address: &str) {
        let _ = self
            .rpc_call("generatetoaddress", json!([blocks, address]))
            .await
            .expect("generatetoaddress RPC should succeed");
    }

    pub async fn send_to_address(&self, address: &str, amount_btc: f64) -> String {
        self.rpc_call("sendtoaddress", json!([address, amount_btc]))
            .await
            .expect("sendtoaddress RPC should succeed")
            .as_str()
            .expect("sendtoaddress result should be a txid string")
            .to_string()
    }

    pub fn rpc_host(&self) -> &'static str {
        "127.0.0.1"
    }

    pub fn rpc_port(&self) -> u16 {
        self.rpc_port
    }

    pub fn rpc_user(&self) -> &str {
        &self.rpc_user
    }

    pub fn rpc_password(&self) -> &str {
        &self.rpc_password
    }

    /// Full RPC URL including the http:// scheme.
    pub fn rpc_url_with_auth(&self) -> String {
        format!(
            "http://{}:{}@127.0.0.1:{}/wallet/{}",
            self.rpc_user, self.rpc_password, self.rpc_port, self.wallet
        )
    }

    async fn wait_until_ready(&self) {
        for _ in 0..60u32 {
            let ready = self.rpc_call("getblockchaininfo", json!([])).await.is_ok();
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        panic!(
            "{} RPC at {} did not become ready in time — is the node running, \
             and are the credentials right?",
            self.label, self.rpc_url
        );
    }

    /// Issue an arbitrary JSON-RPC call, for assertions the typed helpers
    /// above do not cover.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc_call(method, params).await
    }

    /// Issue a JSON-RPC call against a named wallet rather than the node's
    /// default one.
    ///
    /// A node can hold several wallets, and a scenario that funds one from
    /// another needs to say which it means — `rpc` alone talks to the node,
    /// not to a wallet.
    pub async fn rpc_wallet(
        &self,
        wallet: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let url = format!("{}/wallet/{}", self.rpc_url, wallet);
        self.rpc_call_at(&url, method, params).await
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let url = self.rpc_url.clone();
        self.rpc_call_at(&url, method, params).await
    }

    async fn rpc_call_at(
        &self,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let body = json!({
            "jsonrpc": "1.0",
            "id": "test",
            "method": method,
            "params": params,
        });

        let response = self
            .client
            .post(url)
            .basic_auth(&self.rpc_user, Some(&self.rpc_password))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("RPC request failed for {method}: {e}"))?;

        let status = response.status();
        let json: Value = response
            .json()
            .await
            .map_err(|e| format!("RPC response JSON decode failed for {method}: {e}"))?;

        if !status.is_success() {
            return Err(format!("RPC HTTP status error for {method}: {status}, body: {json}"));
        }

        if !json["error"].is_null() {
            return Err(format!("RPC returned error for {method}: {}", json["error"]));
        }

        Ok(json["result"].clone())
    }
}
