# diamond-x-e2e

End-to-end scenarios for **diamond-x**, a decentralised exchange that
swaps BTC against XBT — Bitcoin Knots after its BLAKE2b consensus
split — over Lightning, and the shared harness the other suites use.

## What is here

| Crate | What it is |
|---|---|
| `harness` | `dln-e2e-harness` — regtest backends, a Nostr relay, and a client for the node's control planes |
| `scenarios` | the exchange's own scenarios |

### Scenarios

- `two_chains` — two chains, four nodes, a channel on each: the
  exchange's topology, before any swap runs over it
- `lightning_swap` — an atomic cross-chain swap, both legs settling
  against one payment hash
- `swap_on_xbt` — the same swap with a XBT leg that has actually
  activated BLAKE2b, so one chain serves 164-byte headers and the other
  does not

Each prints `PASS` or `FAIL` and sets its exit status.

```
cargo run --bin lightning_swap
```

## What is not here

Scenarios that test the node rather than the exchange:

| Repo | Covers |
|---|---|
| [`dln-node-e2e`](https://github.com/DarkWebDivingClub/dln-node-e2e) | the node itself — channels, invoices, payments, on-chain |
| [`dln-node-knots-e2e`](https://github.com/DarkWebDivingClub/dln-node-knots-e2e) | the node against a XBT chain, including across activation |

Both depend on this repository's `harness` crate. The dependency runs
that way because the harness lives with its most demanding consumer:
the exchange needs two chains, four nodes and cross-chain assertions,
and everything the other suites need is a subset of that.

## The harness

`dln-e2e-harness` provides:

- **`bitcoind`** — Bitcoin Core and Bitcoin Knots regtest containers,
  including a Knots node that activates BLAKE2b at a chosen height
- **`relay`** — a Nostr relay, which is the node's only control plane
- **`dln_node_client`** — NWC and NCC clients, plus node lifecycle
- **`process`**, **`util`** — child processes, ports, temporary directories

It is a library rather than three copies because it changes often, and
copies drift silently.

## Requirements

Docker, for the regtest and relay containers. Scenarios build the node
under test from a local checkout; `DLN_NODE_BINARY` overrides that, and
`KNOTS_FEATURES` selects the cargo features for the Knots build.

## Licence

GPL-3.0-only.
