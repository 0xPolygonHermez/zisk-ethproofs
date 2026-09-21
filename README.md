# Zisk Ethproofs

## EthProofs Client

The EthProofs client connects to an Ethereum node, generates the block inputs, proves each block with ZisK and, optionally, submits the proofs to EthProofs. It is configured entirely through command-line flags.

### Build

To build `ethproofs-client`, run:

```bash
cargo build --release
```

### Run

The client needs a running ZisK Cluster and access to an Ethereum full node (HTTP and WebSocket JSON-RPC) that supports the `debug_executionWitness` endpoint, required to generate the block inputs (Reth full node supports it). By default it connects to a local coordinator (`http://localhost:50051`) and a local node (`http://localhost:8545` / `ws://localhost:8546`), generates inputs from RPC and proves every new block:

```bash
target/release/ethproofs-client
```

To also submit the generated proofs to EthProofs, enable submission and provide the API credentials and cluster ID:

```bash
target/release/ethproofs-client \
    --ethproofs.submit \
    --ethproofs.api-url <API_URL> \
    --ethproofs.api-token <API_TOKEN> \
    --ethproofs.cluster-id <CLUSTER_ID>
```

### Relevant flags

| Flag | Description |
|------|-------------|
| `-g, --guest <PATH>` | Path to the guest ELF file (default `./elf/zec-reth.elf`) |
| `-c, --coordinator-url <URL>` | Zisk coordinator URL (default `http://localhost:50051`) |
| `--rpc.http-url <URL>` | Ethereum node HTTP RPC URL (default `http://localhost:8545`) |
| `--rpc.ws-url <URL>` | Ethereum node WebSocket RPC URL (default `ws://localhost:8546`) |
| `--input.block-modulus <N>` | Only process blocks whose number is a multiple of this value (default `1`) |
| `--compressed` | Wrap the final proof into its compressed (size-optimized) variant |
| `-s, --ethproofs.submit` | Submit proofs to EthProofs. Requires `--ethproofs.api-url`, `--ethproofs.api-token` and `--ethproofs.cluster-id` |
| `--ethproofs.api-url <URL>` | EthProofs API URL |
| `--ethproofs.api-token <TOKEN>` | EthProofs API token |
| `--ethproofs.cluster-id <ID>` | EthProofs cluster ID where proofs are submitted |

Run `target/release/ethproofs-client --help` to see the full list of available flags.
