#!/usr/bin/env bash
#
# Build and start a local ZisK cluster (coordinator + one worker), then run
# ethproofs-client against it to prove blocks — either following the chain over
# RPC or replaying pre-generated inputs from a folder.
#
# All three components log to .run/; the coordinator log is streamed to the
# terminal. Ctrl-C, a closed terminal or a closed pipe stops all three.
#
# Run with --help for the full flag list.

set -euo pipefail

# --- Defaults ---------------------------------------------------------------

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

ELF=""
CLIENT="reth"
HINTS=0
GPU=1
EMULATOR=0
ZISK_DIR="$HOME/zisk"
PROVING_KEY="$HOME/.zisk/provingKey"
RPC_HTTP="http://157.180.1.98:8545"
RPC_WS="ws://157.180.1.98:8546"
INPUT_FOLDER=""
RPC_SET=0
EXTRA_CLIENT_ARGS=()

# Ports, as documented in zisk's distributed/README.md.
API_PORT=7000      # coordinator, client-facing gRPC
CLUSTER_PORT=50051 # coordinator, worker-facing gRPC
METRICS_PORT=9090  # coordinator, Prometheus

HINTS_SOCKET="/tmp/hints.sock"
LOG_DIR="$REPO_ROOT/.run"

COORDINATOR_READY_TIMEOUT=30 # seconds to wait for the metrics endpoint
# A worker with a cold cache regenerates its constant trees before registering,
# which takes several minutes, so this wait is deliberately generous.
WORKER_READY_TIMEOUT=900
STOP_GRACE=10 # seconds between SIGTERM and SIGKILL

# --- Output helpers ---------------------------------------------------------

if [[ -t 1 ]]; then
    C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_RED=$'\033[31m'; C_BLUE=$'\033[34m'
else
    C_RESET=""; C_BOLD=""; C_RED=""; C_BLUE=""
fi

# Tolerates a dead stdout: during shutdown the terminal or pipe may already be
# gone, and a failed write must not abort the cleanup that is still in progress.
info() { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*" 2>/dev/null || true; }
die() { printf '%serror:%s %s\n' "$C_RED$C_BOLD" "$C_RESET" "$*" >&2; exit 1; }

usage() {
    cat <<EOF
${C_BOLD}Usage:${C_RESET} $(basename "${BASH_SOURCE[0]}") --elf <PATH> [options] [-- <client args>...]

Builds zisk-coordinator, zisk-worker and ethproofs-client, starts the coordinator
and one worker, then runs the client against them to prove blocks.

${C_BOLD}Required:${C_RESET}
  --elf <PATH>          Guest ELF file, passed to the client as --guest.

${C_BOLD}Options:${C_RESET}
  --client <NAME>       Execution client: reth | ethrex | ziskethone (default: $CLIENT).
  --hints               Build ethproofs-client with --cfg=zisk_hints and stream
                        hints over $HINTS_SOCKET. Uses its own target dir, so
                        toggling this flag does not force a rebuild.
  --emulator            Run the worker with -l/--emulator (prebuilt emulator)
                        instead of the ASM backend. Cannot be combined
                        with --hints, which requires ASM.
  --no-gpu              Run the worker without -g/--gpu (GPU is on by default).
  --zisk-dir <PATH>     zisk checkout to build the coordinator and worker from
                        (default: $ZISK_DIR).
  --proving-key <PATH>  Worker proving-key folder (default: $PROVING_KEY).
  --inputs <DIR>        Replay pre-generated input files from DIR instead of
                        following the chain over RPC. Mutually exclusive
                        with --rpc-http / --rpc-ws.
  --rpc-http <URL>      Ethereum node HTTP RPC (default: $RPC_HTTP).
  --rpc-ws <URL>        Ethereum node WebSocket RPC (default: $RPC_WS).
  -h, --help            Show this help.

Everything after a bare -- is appended verbatim to the ethproofs-client command
line, e.g. -- --run-time 60 --proof.csv proofs.csv

${C_BOLD}Logs:${C_RESET} all three components write to $LOG_DIR/
  coordinator.log       streamed to the terminal while the client runs
  worker.log            file only — tail it yourself if you need it
  ethproofs-client.log  file only — tail it yourself if you need it
EOF
}

# --- Argument parsing -------------------------------------------------------

while [[ $# -gt 0 ]]; do
    case "$1" in
        --elf)         [[ $# -ge 2 ]] || die "--elf needs a value"; ELF="$2"; shift 2 ;;
        --client)      [[ $# -ge 2 ]] || die "--client needs a value"; CLIENT="$2"; shift 2 ;;
        --hints)       HINTS=1; shift ;;
        --emulator)    EMULATOR=1; shift ;;
        --no-gpu)      GPU=0; shift ;;
        --zisk-dir)    [[ $# -ge 2 ]] || die "--zisk-dir needs a value"; ZISK_DIR="$2"; shift 2 ;;
        --proving-key) [[ $# -ge 2 ]] || die "--proving-key needs a value"; PROVING_KEY="$2"; shift 2 ;;
        --inputs)      [[ $# -ge 2 ]] || die "--inputs needs a value"; INPUT_FOLDER="$2"; shift 2 ;;
        --rpc-http)    [[ $# -ge 2 ]] || die "--rpc-http needs a value"; RPC_HTTP="$2"; RPC_SET=1; shift 2 ;;
        --rpc-ws)      [[ $# -ge 2 ]] || die "--rpc-ws needs a value"; RPC_WS="$2"; RPC_SET=1; shift 2 ;;
        -h|--help)     usage; exit 0 ;;
        --)            shift; EXTRA_CLIENT_ARGS=("$@"); break ;;
        *)             usage >&2; die "unknown argument: $1" ;;
    esac
done

# --- Validation -------------------------------------------------------------

[[ -n "$ELF" ]] || { usage >&2; die "--elf is required"; }
[[ -f "$ELF" ]] || die "ELF not found: $ELF"
ELF="$(cd -- "$(dirname -- "$ELF")" && pwd)/$(basename -- "$ELF")"

case "$CLIENT" in
    reth|ethrex|ziskethone) ;;
    *) die "--client must be one of: reth, ethrex, ziskethone (got '$CLIENT')" ;;
esac

# Hints require the ASM backend, and --emulator is the alternative to it, so
# the two can never be combined.
if (( HINTS == 1 && EMULATOR == 1 )); then
    die "--hints requires the ASM backend and cannot be used with --emulator"
fi

if [[ -n "$INPUT_FOLDER" ]]; then
    (( RPC_SET == 0 )) || die "--inputs replays from disk and cannot be combined with --rpc-http/--rpc-ws"
    [[ -d "$INPUT_FOLDER" ]] || die "input folder not found: $INPUT_FOLDER"
    INPUT_FOLDER="$(cd -- "$INPUT_FOLDER" && pwd)"
    shopt -s nullglob
    input_files=("$INPUT_FOLDER"/*.bin)
    shopt -u nullglob
    (( ${#input_files[@]} > 0 )) || die "no .bin input files in $INPUT_FOLDER"
fi

[[ -d "$ZISK_DIR" ]] || die "zisk directory not found: $ZISK_DIR (set --zisk-dir)"
ZISK_DIR="$(cd -- "$ZISK_DIR" && pwd)"
[[ -f "$ZISK_DIR/Cargo.toml" ]] || die "$ZISK_DIR does not look like a zisk checkout (no Cargo.toml)"

COORDINATOR_BIN="$ZISK_DIR/target/release/zisk-coordinator"
WORKER_BIN="$ZISK_DIR/target/release/zisk-worker"

[[ -d "$PROVING_KEY" ]] || die "proving key folder not found: $PROVING_KEY (set --proving-key)"
PROVING_KEY="$(cd -- "$PROVING_KEY" && pwd)"

# Name the offending process, since a stale coordinator is the usual cause and
# lsof is not installed everywhere.
port_holder() { ss -H -ltnp "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -n 1; }
for port in "$API_PORT" "$CLUSTER_PORT" "$METRICS_PORT"; do
    ss -H -ltn "sport = :$port" 2>/dev/null | grep -q . || continue
    holder="$(port_holder "$port")"
    if [[ -n "$holder" ]]; then
        die "port $port is already in use by pid $holder ($(tr '\0' ' ' < "/proc/$holder/cmdline" 2>/dev/null))
    stop it with: kill $holder"
    fi
    die "port $port is already in use — a stale coordinator is probably still running"
done

# --- Build ------------------------------------------------------------------

# The hints build of the client differs only by a cfg flag, so it gets its own
# target dir: toggling --hints then reuses a warm cache instead of recompiling
# the whole dependency tree every time.
if [[ $HINTS -eq 1 ]]; then
    CLIENT_TARGET_DIR="$REPO_ROOT/target/hints"
else
    CLIENT_TARGET_DIR="$REPO_ROOT/target"
fi
CLIENT_BIN="$CLIENT_TARGET_DIR/release/ethproofs-client"

# zisk goes first — it is by far the slower build, and CUDA support is
# auto-detected here, so the worker picks up the GPU with no feature flag.
info "Building zisk-coordinator and zisk-worker in $ZISK_DIR..."
( cd -- "$ZISK_DIR" && cargo build --release --bin zisk-coordinator --bin zisk-worker )

if [[ $HINTS -eq 1 ]]; then
    info "Building ethproofs-client (with hints)..."
    RUSTFLAGS="--cfg=zisk_hints" CARGO_TARGET_DIR="$CLIENT_TARGET_DIR" \
        cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
else
    info "Building ethproofs-client..."
    CARGO_TARGET_DIR="$CLIENT_TARGET_DIR" \
        cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
fi

for bin in "$COORDINATOR_BIN" "$WORKER_BIN" "$CLIENT_BIN"; do
    [[ -x "$bin" ]] || die "build finished but $bin is missing"
done

# --- Process management -----------------------------------------------------

COORDINATOR_PID=""
WORKER_PID=""
CLIENT_PID=""
TAIL_PID=""
CLEANED_UP=0

mkdir -p "$LOG_DIR"
COORDINATOR_LOG="$LOG_DIR/coordinator.log"
WORKER_LOG="$LOG_DIR/worker.log"
CLIENT_LOG="$LOG_DIR/ethproofs-client.log"

# An exited-but-unwaited child still answers `kill -0`, so check for the zombie
# state too — otherwise the supervision loop below never notices an exit.
alive() {
    local pid="$1" state
    [[ -n "$pid" ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)" || return 1
    [[ "$state" != "Z" ]]
}

# The children deliberately stay in this script's process group: a terminal
# hangup then reaches them even if the script dies without running cleanup.
stop_proc() {
    local pid="$1" name="$2" waited=0
    alive "$pid" || return 0
    info "Stopping $name (pid $pid)..."
    pkill -TERM -P "$pid" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    while alive "$pid" && (( waited < STOP_GRACE * 2 )); do
        sleep 0.5
        waited=$((waited + 1))
    done
    if alive "$pid"; then
        pkill -KILL -P "$pid" 2>/dev/null || true
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}

# Trapped on every signal that can end this script, not just INT/TERM: a closed
# pipe (PIPE) or a closed terminal (HUP) used to leave the coordinator running.
cleanup() {
    (( CLEANED_UP == 0 )) || return 0
    CLEANED_UP=1
    # Ignore these rather than restoring their defaults: with stdout gone,
    # a default SIGPIPE would kill the script mid-cleanup and orphan the
    # coordinator — exactly the leak this trap exists to prevent.
    trap '' INT TERM HUP QUIT PIPE
    trap - EXIT
    stop_proc "$CLIENT_PID" "ethproofs-client"
    stop_proc "$WORKER_PID" "worker"
    stop_proc "$COORDINATOR_PID" "coordinator"
    if [[ -n "$TAIL_PID" ]]; then
        kill -TERM "$TAIL_PID" 2>/dev/null || true
        wait "$TAIL_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM HUP QUIT PIPE

# --- Start the coordinator --------------------------------------------------

info "Starting coordinator (api :$API_PORT, cluster :$CLUSTER_PORT, metrics :$METRICS_PORT)"
"$COORDINATOR_BIN" \
    --api-port "$API_PORT" \
    --cluster-port "$CLUSTER_PORT" \
    --metrics-port "$METRICS_PORT" \
    >"$COORDINATOR_LOG" 2>&1 &
COORDINATOR_PID=$!
info "  pid $COORDINATOR_PID, log $COORDINATOR_LOG"

metrics() { curl -fsS --max-time 2 "http://127.0.0.1:$METRICS_PORT/metrics" 2>/dev/null; }

for (( i = 0; i < COORDINATOR_READY_TIMEOUT * 2; i++ )); do
    alive "$COORDINATOR_PID" || die "coordinator exited during startup — see $COORDINATOR_LOG"
    metrics >/dev/null && break
    sleep 0.5
done
metrics >/dev/null || die "coordinator did not answer on :$METRICS_PORT within ${COORDINATOR_READY_TIMEOUT}s — see $COORDINATOR_LOG"
info "Coordinator ready"

# --- Start the worker -------------------------------------------------------

worker_args=(--coordinator-url "http://localhost:$CLUSTER_PORT" --proving-key "$PROVING_KEY")
if (( GPU == 1 )); then
    worker_args+=(--gpu)
    gpu_state="on"
else
    gpu_state="off"
fi
if (( EMULATOR == 1 )); then
    worker_args+=(--emulator)
    backend="emulator"
else
    backend="asm"
fi

info "Starting worker (backend $backend, gpu $gpu_state, proving key $PROVING_KEY)"
"$WORKER_BIN" "${worker_args[@]}" >"$WORKER_LOG" 2>&1 &
WORKER_PID=$!
info "  pid $WORKER_PID, log $WORKER_LOG"

# The gauge is absent until the first worker connects, so an empty read counts
# as "not ready yet" rather than an error.
workers_connected() {
    metrics | awk '$1 == "coordinator_workers_connected" { print $2; exit }'
}

info "Waiting for the worker to register (first run regenerates constant trees, this takes a few minutes)..."
worker_registered=0
for (( i = 0; i < WORKER_READY_TIMEOUT * 2; i++ )); do
    alive "$WORKER_PID" || die "worker exited during startup — see $WORKER_LOG"
    alive "$COORDINATOR_PID" || die "coordinator exited during worker startup — see $COORDINATOR_LOG"
    connected="$(workers_connected)"
    if [[ -n "$connected" ]] && awk -v v="$connected" 'BEGIN { exit !(v >= 1) }'; then
        worker_registered=1
        break
    fi
    # Every 30s, show the worker is making progress rather than hanging.
    if (( i > 0 && i % 60 == 0 )); then
        info "  still waiting ($((i / 2))s): $(tail -n 1 "$WORKER_LOG" 2>/dev/null)"
    fi
    sleep 0.5
done
(( worker_registered == 1 )) || die "worker did not register within ${WORKER_READY_TIMEOUT}s — see $WORKER_LOG and $COORDINATOR_LOG"
info "Worker registered with the coordinator"

# --- Start ethproofs-client -------------------------------------------------

# --coordinator-url must be the client-facing API port; the client's own default
# is the worker-facing one, which would never connect.
client_args=(
    --guest "$ELF"
    --client "$CLIENT"
    --coordinator-url "http://localhost:$API_PORT"
)
if [[ -n "$INPUT_FOLDER" ]]; then
    client_args+=(--input.mode folder --folder.path "$INPUT_FOLDER")
    source_desc="${#input_files[@]} input file(s) from $INPUT_FOLDER"
else
    client_args+=(--rpc.http-url "$RPC_HTTP" --rpc.ws-url "$RPC_WS")
    source_desc="rpc $RPC_HTTP"
fi
if [[ $HINTS -eq 1 ]]; then
    # A socket left behind by a previous run would make the bind fail.
    rm -f "$HINTS_SOCKET"
    client_args+=(--hints.mode socket --hints.socket "$HINTS_SOCKET")
fi
client_args+=("${EXTRA_CLIENT_ARGS[@]+"${EXTRA_CLIENT_ARGS[@]}"}")

info "Starting ethproofs-client (elf $ELF, client $CLIENT, $source_desc)"
info "Below is the coordinator log. Client: $CLIENT_LOG — worker: $WORKER_LOG"
echo

# Only the coordinator is shown; the client and worker go to their log files.
tail -n 0 -F "$COORDINATOR_LOG" 2>/dev/null &
TAIL_PID=$!

"$CLIENT_BIN" "${client_args[@]}" >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

# --- Supervise --------------------------------------------------------------

client_status=0
while true; do
    if ! alive "$CLIENT_PID"; then
        wait "$CLIENT_PID" || client_status=$?
        break
    fi
    if ! alive "$COORDINATOR_PID"; then
        echo
        die "coordinator died unexpectedly — see $COORDINATOR_LOG"
    fi
    if ! alive "$WORKER_PID"; then
        echo
        die "worker died unexpectedly — see $WORKER_LOG"
    fi
    sleep 1
done

echo
info "ethproofs-client exited with status $client_status"
cleanup
exit "$client_status"
