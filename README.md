# Zisk Ethproofs

## EthProofs Client

### Setup

Copy the file `bin/ethproofs-client/example.env` to `.env`, then edit it to set the environment variables:

| Variable               | Description                                                                 |
|------------------------|-----------------------------------------------------------------------------|
| `INPUT_GEN_SERVER_URL` | URL of the input-gen-server                                                 |
| `WEBHOOK_PORT`         | Listening port for the Webhook server                                       |
| `METRICS_PORT`         | Listening port for the Prometheus metrics server                            |
| `COORDINATOR_URL`      | URL of the Zisk Coordinator                                                 |
| `ETHPROOFS_API_URL`    | URL of the EthProofs API                                                    |
| `ETHPROOFS_API_TOKEN`  | Token used to authenticate with the EthProofs API                           |
| `ETHPROOFS_CLUSTER`    | EthProofs cluster ID where proofs will be submitted                         |
| `TELEGRAM_BOT_TOKEN`   | Telegram Bot API token for sending alerts                                   |
| `TELEGRAM_CHAT_ID`     | Telegram chat ID where alerts will be sent                                  |
| `INPUTS_FOLDER`        | Folder where input files will be stored                                     |
| `COMPUTE_CAPACITY`     | Compute capacity required for block proofs                                  |

### Build

To build `ethproofs-client`, run:

```bash
cargo build --release --bin ethproofs-client
```

### Run

To run `ethproofs-client` and start submitting block proofs to EthProofs, run:

```bash
target/release/ethproofs-client -s
```

### Command-line Flags
```
  -s, --submit-ethproofs
          Enable proof submission to Ethproofs
  -t, --telegram-alert <TELEGRAM_ALERT>...
          Send Telegram alerts for specified events [possible values: block-proved, skipped-threshold, proof-failed]
  -d, --insert-db
          Insert block proof data into the database
  -k, --skip-proving
          Skip the proving step (useful for testing)
  -m, --enable-metrics
          Enable Prometheus metrics server
  -i, --keep-input
          Keep the input file after processing
  -b, --skipped-threshold <SKIPPED_THRESHOLD>
          Number of skipped blocks before triggering an alert [default: 5]
  -p, --panic-on-skipped
          Panic when skipped blocks exceed the threshold
```

---

## Input Generator Server

### Setup

Copy the file `bin/input-gen-server/example.env` to `.env`, then edit it to set the environment variables:

| Variable             | Description                                                                 |
|----------------------|-----------------------------------------------------------------------------|
| `RPC_URL`            | HTTP JSON-RPC URL for Ethereum Mainnet                                      |
| `RPC_WS_URL`         | WebSocket JSON-RPC URL for Ethereum Mainnet                                 |
| `WS_PORT`            | Listening port for the input-gen-server WebSocket server                    |
| `INPUTS_FOLDER`      | Folder where the block input files are stored                               |
| `BLOCK_MODULUS`      | Modulus used to select which blocks will generate input files               |

### Build

To build `input-gen-server`, run:

```bash
cargo build --release --bin input-gen-server
```

### Run

To run `input-gen-server` and start generating block inputs for the `zec-rsp.elf` client, run:

```bash
target/release/input-gen-server
```

### Command-line Flags
```
  -g, --guest <GUEST>  Guest program for which to generate inputs [default: rsp] [possible values: rsp, zeth]
```
