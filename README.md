# Zisk-Ethproofs

## Setup

Copy the `example.env` file to `.env`, then edit it to set the required variables:

| Variable                     | Description                                                                 | Required | Default |
|------------------------------|-----------------------------------------------------------------------------|----------|---------|
| `RPC_URL`                    | HTTP JSON-RPC URL for the Ethereum Mainnet node                             | Yes      | –       |
| `RPC_WS_URL`                 | WebSocket JSON-RPC URL for the Ethereum Mainnet node                        | Yes      | –       |
| `ETHPROOFS_API_URL`          | URL of the Ethproofs API                                                    | Yes      | –       |
| `ETHPROOFS_API_TOKEN`        | Token for accessing the Ethproofs API                                       | Yes      | –       |
| `ETHPROOFS_CLUSTER`          | Ethproofs cluster ID where proofs will be submitted                         | Yes      | –       |
| `BLOCK_MODULUS`              | Modulus value used to select which blocks will be proven                    | No       | `100`   |
| `ELF_FILE`                   | Name of the ELF file used to prove blocks. It must be located in the `./program` folder  | Yes | -     |
| `DISTRIBUTED_PROVE_PROCESSES`| Number of processes used for distributed proving                            | Yes      | –       |
| `DISTRIBUTED_PROVE_THREADS`  | Number of threads per process for distributed proving                       | Yes      | –       |
| `TELEGRAM_BOT_TOKEN`         | Telegram Bot API token for sending alerts. Alerts are disabled if undefined | No       | –       |
| `TELEGRAM_CHAT_ID`           | Telegram chat ID where alerts will be sent. Alerts are disabled if undefined| No       | –       |

## Install

To install Zisk-ethproofs, use the following command:
```bash
cargo install --path .
```

## Run

To run Zisk-Ethproofs and start submitting block proofs to Ethproofs, use the following command:

```bash
zisk-ethproofs
```

### Command-line flags

| Flag | Description |
|------|-------------|
| `-n, --no-ethproofs` | Disable proof status reporting and submission to Ethproofs. Only block proving will be performed |
| `-b, --block-submit-alert` | Send a Telegram alert when a block proof is successfully submitted to Ethproofs. By default, only error/warning alerts are sent |
| `-t, --test-block <TEST_BLOCK>` | Generate and submit the proof for a specific block only. Useful for testing or troubleshooting. Example: `-t 22137695` |
| `-d, --disable-distributed` | Disable distributed proving. The proof will be generated using a single process |
| `-k, --keep-input` | Keeps the block input file after proof generation (does not delete it) |

### Folders

The following folders will be created during the execution of Zisk-Ethproofs:

| Folder       | Description                                                                                 |
|--------------|---------------------------------------------------------------------------------------------|
| `./input`    | Stores the input file generated for each block                                              |
| `./proof`    | Stores the proof files generated for each block. A subfolder named after the block number will be created for each block |
| `./log`      | Stores the log file generated for each block                                                |
