#!/usr/bin/env bash
# gameday.sh — repeatable local DR drill for QFC (T4.3).
#
# Drill, in one run:
#   Path A (snapshot restore):
#     start a dev node with frequent snapshots -> wait for >=N archives and
#     blocks -> record H_snap (latest archive) and H_kill (chain head) ->
#     SIGKILL the node and DESTROY its datadir -> restore from the latest
#     archive via scripts/restore.sh -> restart -> measure
#       RPO = H_kill - H_snap   (blocks lost)
#       RTO = wall time from datadir destruction to RPC serving an
#             advancing block number
#   Path B (T4.1 consensus-checkpoint fast restart):
#     clean stop of the recovered node -> restart on the intact datadir ->
#     measure restart-to-advancing-RPC time (RPO = 0, nothing lost).
#
# Everything runs in a throwaway temp workdir; snapshot archives are written
# OUTSIDE the datadir (simulating off-box object storage), so destroying the
# datadir models a real disk loss. The only rm -rf targets are inside the
# workdir and are guarded by an explicit path check.
#
# See docs/DR.md for the runbook and the measured numbers from past runs.
set -euo pipefail

usage() {
  cat <<'EOF'
gameday.sh — local QFC disaster-recovery drill with measured RPO/RTO

USAGE:
  gameday.sh [OPTIONS]

OPTIONS:
  --node-bin PATH        qfc-node binary (default: <repo>/target/release/qfc-node;
                         built with `cargo build --release -p qfc-node` if missing).
  --no-build             Fail instead of building when the binary is missing.
  --snapshot-interval S  --snapshot-interval-secs for the drill node (default 15).
  --min-snapshots N      Archives required before pulling the plug (default 2).
  --min-blocks N         Chain height required before pulling the plug (default 12).
  --rpc-port P           Drill node RPC port (default 18545).
  --metrics-port P       Drill node metrics port (default 16060).
  --keep                 Keep the workdir (logs, archives) after the drill.
  -h, --help             This help.

The drill needs ~1-2 minutes and exclusive use of the two ports.

EXAMPLE:
  scripts/gameday.sh --snapshot-interval 15 --min-blocks 12
EOF
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

NODE_BIN="$REPO_ROOT/target/release/qfc-node"
NO_BUILD=0
SNAP_INTERVAL=15
MIN_SNAPSHOTS=2
MIN_BLOCKS=12
RPC_PORT=18545
METRICS_PORT=16060
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --node-bin)          NODE_BIN="${2:?}"; shift 2 ;;
    --no-build)          NO_BUILD=1; shift ;;
    --snapshot-interval) SNAP_INTERVAL="${2:?}"; shift 2 ;;
    --min-snapshots)     MIN_SNAPSHOTS="${2:?}"; shift 2 ;;
    --min-blocks)        MIN_BLOCKS="${2:?}"; shift 2 ;;
    --rpc-port)          RPC_PORT="${2:?}"; shift 2 ;;
    --metrics-port)      METRICS_PORT="${2:?}"; shift 2 ;;
    --keep)              KEEP=1; shift ;;
    -h|--help)           usage; exit 0 ;;
    *) printf 'unknown option: %s\n' "$1" >&2; usage >&2; exit 1 ;;
  esac
done

log()  { printf '[gameday] %s\n' "$*" >&2; }
fail() { printf '[gameday] ERROR: %s\n' "$*" >&2; exit 1; }

now_ms() {
  if command -v perl >/dev/null; then
    perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000'
  else
    printf '%s000\n' "$(date +%s)" # 1s resolution fallback
  fi
}

RPC_URL="http://127.0.0.1:$RPC_PORT"
rpc_bn() { # prints decimal block number, or nothing if RPC is down
  local hex
  hex="$(curl -sf --max-time 3 "$RPC_URL" -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    | sed -n 's/.*"result" *: *"0x\([0-9a-fA-F]*\)".*/\1/p' || true)"
  if [ -n "$hex" ]; then printf '%d\n' "0x$hex"; fi
}

# Wait until the RPC block number advances at least once; echoes the
# advancing height. $1 = timeout seconds.
wait_advancing() {
  local deadline first bn
  deadline=$(( $(date +%s) + $1 ))
  first=""
  while [ "$(date +%s)" -lt "$deadline" ]; do
    bn="$(rpc_bn)"
    if [ -n "$bn" ]; then
      if [ -z "$first" ]; then
        first="$bn"
      elif [ "$bn" -gt "$first" ]; then
        printf '%s\n' "$bn"
        return 0
      fi
    fi
    sleep 0.5
  done
  return 1
}

# ---------------------------------------------------------------- workspace
if [ ! -x "$NODE_BIN" ]; then
  if [ "$NO_BUILD" = 1 ]; then
    fail "node binary missing: $NODE_BIN (and --no-build given)"
  fi
  log "Building release node binary (cargo build --release -p qfc-node)..."
  (cd "$REPO_ROOT" && cargo build --release -p qfc-node) || fail "build failed"
fi
[ -x "$NODE_BIN" ] || fail "node binary still missing: $NODE_BIN"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/qfc-gameday.XXXXXX")"
touch "$WORKDIR/.qfc-gameday"
DATADIR="$WORKDIR/datadir"
BACKUPS="$WORKDIR/backups"   # outside the datadir = "off-box" object storage
mkdir -p "$DATADIR" "$BACKUPS"
NODE_PID=""

# The ONLY rm -rf in this script. Refuses anything that is not a
# subdirectory of a marker-carrying qfc-gameday workdir.
guarded_rm_rf() {
  local target="$1"
  case "$target" in
    "$WORKDIR"/*) : ;;
    *) fail "guarded_rm_rf refused: '$target' is not inside workdir '$WORKDIR'" ;;
  esac
  case "$WORKDIR" in
    */qfc-gameday.*) : ;;
    *) fail "guarded_rm_rf refused: workdir '$WORKDIR' lacks the qfc-gameday name pattern" ;;
  esac
  [ -f "$WORKDIR/.qfc-gameday" ] || fail "guarded_rm_rf refused: marker file missing in '$WORKDIR'"
  rm -rf "$target"
}

stop_node() {
  if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
    kill -KILL "$NODE_PID" 2>/dev/null || true
    wait "$NODE_PID" 2>/dev/null || true
  fi
  NODE_PID=""
}

cleanup() {
  stop_node
  if [ -f "$WORKDIR/node.pid" ]; then
    kill -KILL "$(cat "$WORKDIR/node.pid")" 2>/dev/null || true
  fi
  if [ "$KEEP" = 1 ]; then
    log "Workdir kept: $WORKDIR"
  else
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

start_node() { # $1 = log file; starts the drill node in the background
  "$NODE_BIN" --dev --no-network \
    --datadir "$DATADIR" \
    --rpc-addr "127.0.0.1:$RPC_PORT" \
    --metrics-addr "127.0.0.1:$METRICS_PORT" \
    --snapshot-interval-secs "$SNAP_INTERVAL" \
    --snapshot-dir "$BACKUPS" \
    --snapshot-retain 10 \
    >>"$1" 2>&1 &
  NODE_PID=$!
}

# restore.sh --start-cmd hook: must survive restore.sh exiting, so write a
# tiny helper that detaches the node and records its pid.
cat >"$WORKDIR/start-node.sh" <<EOF
#!/bin/sh
nohup "$NODE_BIN" --dev --no-network \\
  --datadir "$DATADIR" \\
  --rpc-addr "127.0.0.1:$RPC_PORT" \\
  --metrics-addr "127.0.0.1:$METRICS_PORT" \\
  --snapshot-interval-secs "$SNAP_INTERVAL" \\
  --snapshot-dir "$BACKUPS" \\
  --snapshot-retain 10 \\
  >>"$WORKDIR/node-restored.log" 2>&1 &
echo \$! > "$WORKDIR/node.pid"
EOF
chmod +x "$WORKDIR/start-node.sh"

archive_count() {
  find "$BACKUPS" -maxdepth 1 -name 'qfc-snapshot-*.tar.gz' 2>/dev/null | wc -l | tr -d ' '
}

latest_archive() {
  # shellcheck disable=SC2012  # archive names are <prefix>-<digits>-<digits>.tar.gz, ls -t is safe
  ls -t "$BACKUPS"/qfc-snapshot-*.tar.gz 2>/dev/null | head -1
}

# =============================================================== Path A: drill
log "Workdir: $WORKDIR"
log "PATH A — snapshot restore drill"
log "Starting dev node (snapshots every ${SNAP_INTERVAL}s into $BACKUPS)..."
start_node "$WORKDIR/node-initial.log"

wait_advancing 60 >/dev/null || fail "node never started producing blocks (see $WORKDIR/node-initial.log)"
log "Node up and producing blocks (pid $NODE_PID)"

deadline=$(( $(date +%s) + MIN_SNAPSHOTS * SNAP_INTERVAL + 120 ))
while :; do
  count="$(archive_count)"
  bn="$(rpc_bn)"
  if [ "$count" -ge "$MIN_SNAPSHOTS" ] && [ -n "$bn" ] && [ "$bn" -ge "$MIN_BLOCKS" ]; then
    break
  fi
  [ "$(date +%s)" -lt "$deadline" ] || fail "timed out waiting for $MIN_SNAPSHOTS snapshots + height $MIN_BLOCKS (have $count snapshots, height ${bn:-?})"
  sleep 1
done
log "Preconditions met: $count snapshot archive(s), chain height $bn"

ARCHIVE="$(latest_archive)"
[ -n "$ARCHIVE" ] || fail "no snapshot archive found"
# Name is <prefix>-<height>-<unix_ts>.tar.gz; parse from the right so a
# prefix containing dashes cannot break the parse.
stem="$(basename "$ARCHIVE" .tar.gz)"
ts_part="${stem##*-}"
H_SNAP="${stem%-*}"; H_SNAP="${H_SNAP##*-}"
H_KILL="$(rpc_bn)"
[ -n "$H_KILL" ] || fail "could not read chain height before the kill"
log "Latest archive: $(basename "$ARCHIVE") (H_snap=$H_SNAP, taken at unix $ts_part)"
log "Chain height at kill time: H_kill=$H_KILL"

log "DISASTER: SIGKILL node (pid $NODE_PID) and destroy $DATADIR"
stop_node
T_DESTROY_MS="$(now_ms)"
guarded_rm_rf "$DATADIR"

log "Restoring via scripts/restore.sh from $BACKUPS ..."
"$SCRIPT_DIR/restore.sh" \
  --datadir "$DATADIR" \
  --rpc-url "$RPC_URL" \
  --start-cmd "$WORKDIR/start-node.sh" \
  --validate-timeout 120 \
  "$BACKUPS" \
  || fail "restore.sh failed — drill aborted (workdir: $WORKDIR; rerun with --keep to inspect)"
T_RECOVERED_MS="$(now_ms)"
NODE_PID="$(cat "$WORKDIR/node.pid")"

H_RESTORED="$(rpc_bn)"
[ -n "$H_RESTORED" ] || fail "restored node RPC unreachable after validation (unexpected)"
[ "$H_RESTORED" -ge "$H_SNAP" ] || fail "restored height $H_RESTORED < snapshot height $H_SNAP — restore bug"
RTO_A_MS=$(( T_RECOVERED_MS - T_DESTROY_MS ))
RPO_A_BLOCKS=$(( H_KILL - H_SNAP ))
log "PATH A done: RPO=$RPO_A_BLOCKS blocks, RTO=${RTO_A_MS}ms, height now $H_RESTORED"

# ====================================================== Path B: fast restart
log "PATH B — T4.1 consensus-checkpoint fast restart (clean stop, intact datadir)"
sleep 3   # let the restored node settle / write a checkpoint or two
if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
  kill -TERM "$NODE_PID" 2>/dev/null || true
  for _ in $(seq 1 20); do
    kill -0 "$NODE_PID" 2>/dev/null || break
    sleep 0.5
  done
  kill -KILL "$NODE_PID" 2>/dev/null || true
fi
NODE_PID=""

T_RESTART_MS="$(now_ms)"
start_node "$WORKDIR/node-fastrestart.log"
H_AFTER_RESTART="$(wait_advancing 120)" || fail "node did not serve advancing blocks after clean restart (see $WORKDIR/node-fastrestart.log)"
T_SERVING_MS="$(now_ms)"
RTO_B_MS=$(( T_SERVING_MS - T_RESTART_MS ))

CHECKPOINT_LINE="$(grep -m1 'Loaded validator checkpoint' "$WORKDIR/node-fastrestart.log" || true)"
if [ -n "$CHECKPOINT_LINE" ]; then
  log "Fast-restart confirmed: ${CHECKPOINT_LINE#*qfc_chain::chain: }"
else
  log "WARNING: no 'Loaded validator checkpoint' in restart log — node fell back to genesis validators + full replay"
fi
log "PATH B done: RPO=0 blocks, RTO=${RTO_B_MS}ms, height now $H_AFTER_RESTART"
stop_node

# ===================================================================== report
fmt_s() { printf '%d.%01ds' $(($1 / 1000)) $((($1 % 1000) / 100)); }
cat <<EOF

================================ GAME DAY SUMMARY ================================
  Drill node : --dev --no-network, ${SNAP_INTERVAL}s snapshot interval, 3s blocks
  H_snap=$H_SNAP  H_kill=$H_KILL  (archive: $(basename "$ARCHIVE"))

  Recovery path                       RPO (blocks)   RTO (wall)   Notes
  ----------------------------------  -------------  -----------  -------------------------
  A. snapshot restore (datadir lost)  $RPO_A_BLOCKS              $(fmt_s "$RTO_A_MS")        restore.sh, local archive
  B. checkpoint fast restart          0              $(fmt_s "$RTO_B_MS")        T4.1, datadir intact
  ----------------------------------  -------------  -----------  -------------------------
  RPO upper bound for path A = snapshot interval (${SNAP_INTERVAL}s here;
  production interval determines real-world RPO). Remote-fetch time is NOT
  included in path A (local archive); add object-store download time for a
  true off-site restore.
==================================================================================
EOF
if [ "$KEEP" = 1 ]; then
  log "Logs and archives kept in $WORKDIR"
fi
