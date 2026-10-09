# Zisk Ethproofs

## EthProofs Client

The EthProofs client connects to an Ethereum node, generates the block inputs, proves each block with ZisK and, optionally, submits the proofs to EthProofs. It is configured entirely through command-line flags.

### Build

To build `ethproofs-client`, run:

```bash
cargo build --release
```

### Run

The client needs:

- A running ZisK cluster (`zisk-coordinator` + workers). `--coordinator-url` must point to the coordinator's client-facing API port (`--api-port` of `zisk-coordinator`, `7000` by default).
- An Ethereum full node (HTTP and WebSocket JSON-RPC) that supports the `debug_executionWitness` endpoint, required to generate the block inputs (e.g. a Reth full node).
- The guest ELF of the execution client selected with `--client` (`reth`, `ethrex` or `ziskethone`), built from [zisk-eth-client](https://github.com/0xPolygonHermez/zisk-eth-client).

By default it connects to a local node (`http://localhost:8545` / `ws://localhost:8546`), generates inputs from RPC with the `reth` client and proves every new block:

```bash
target/release/ethproofs-client \
    --coordinator-url http://localhost:7000 \
    --guest ./elf/zec-reth.elf
```

To also submit the generated proofs to EthProofs, enable submission and provide the API credentials and cluster ID:

```bash
target/release/ethproofs-client \
    --coordinator-url http://localhost:7000 \
    --guest ./elf/zec-reth.elf \
    --ethproofs.submit \
    --ethproofs.api-url <API_URL> \
    --ethproofs.api-token <API_TOKEN> \
    --ethproofs.cluster-id <CLUSTER_ID>
```

### Relevant flags

| Flag | Description |
|------|-------------|
| `--client <CLIENT>` | Execution client used to generate the inputs: `reth`, `ethrex` or `ziskethone` (default `reth`). Must match the guest ELF |
| `-g, --guest <PATH>` | Path to the guest ELF file (default `./elf/zec-reth.elf`) |
| `-c, --coordinator-url <URL>` | ZisK coordinator URL, client-facing API port (default `http://localhost:7000`) |
| `--rpc.http-url <URL>` | Ethereum node HTTP RPC URL (default `http://localhost:8545`) |
| `--rpc.ws-url <URL>` | Ethereum node WebSocket RPC URL (default `ws://localhost:8546`) |
| `--input.block-modulus <N>` | Only process blocks whose number is a multiple of this value (default `1`) |
| `-t, --prove-timeout <SECS>` | Timeout for proving a block in seconds (default `600`) |
| `-s, --ethproofs.submit` | Submit proofs to EthProofs. Requires `--ethproofs.api-url`, `--ethproofs.api-token` and `--ethproofs.cluster-id` |
| `--ethproofs.api-url <URL>` | EthProofs API URL |
| `--ethproofs.api-token <TOKEN>` | EthProofs API token |
| `--ethproofs.cluster-id <ID>` | EthProofs cluster ID where proofs are submitted |

Run `target/release/ethproofs-client --help` to see the full list of available flags.
