#!/usr/bin/env bash
# restore.sh — restore a QFC node data directory from a snapshot archive (T4.3).
#
# Counterpart of the T4.2 backup pipeline (--snapshot-interval-secs /
# --snapshot-upload-cmd in qfc-node): consumes the
# `<prefix>-<height>-<unix_ts>.tar.gz` archives it produces.
#
# Flow: fetch -> verify -> place -> (re)start -> validate.
# See docs/DR.md for the full runbook.
set -euo pipefail

usage() {
  cat <<'EOF'
restore.sh — restore a QFC node datadir from a snapshot archive

USAGE:
  restore.sh [OPTIONS] [ARCHIVE]

ARCHIVE
  Path to a local snapshot archive (<prefix>-<height>-<ts>.tar.gz), or a
  directory containing such archives (the newest matching --prefix is used).
  Alternatively fetch from remote storage with --fetch-cmd.

OPTIONS:
  --archive PATH        Same as the positional ARCHIVE argument.
  --fetch-cmd CMD       Shell command that downloads the archive. `{file}`
                        is replaced with the local destination path the
                        command must write to (mirror of the node's
                        --snapshot-upload-cmd convention). Env fallback:
                        RESTORE_FETCH_CMD.
  --datadir DIR         Node data directory to restore into (DB is placed at
                        <datadir>/db). Default: ./data or $QFC_DATA_DIR.
  --prefix NAME         Archive name prefix when ARCHIVE is a directory.
                        Default: qfc-snapshot or $QFC_SNAPSHOT_PREFIX.
  --force               Replace an existing <datadir>/db (it is preserved as
                        <datadir>/db.bak.<timestamp>, never deleted).
  --start-cmd CMD       Command run (via sh -c) after the DB is in place to
                        start the node, e.g. 'systemctl start qfc-node'.
  --rpc-url URL         Node RPC endpoint polled during validation.
                        Default: http://127.0.0.1:8545
  --peer-rpc-url URL    Optional healthy peer RPC; restored height is
                        compared against it to report sync lag.
  --validate            Poll RPC until the block number advances even when
                        no --start-cmd is given (implied by --start-cmd).
  --validate-timeout S  Seconds to wait for an advancing block number
                        (default: 180).
  --verify-only         Fetch + verify the archive, print its manifest
                        summary, and exit without touching the datadir.
  -h, --help            This help.

EXAMPLES:
  # Restore newest local archive into a stopped node's datadir:
  restore.sh --datadir /var/lib/qfc --force /var/lib/qfc/snapshots

  # Fetch from object storage, restore, restart via systemd, validate:
  restore.sh \
    --fetch-cmd 'rclone copyto remote:qfc-backups/qfc-snapshot-1200-1765432100.tar.gz {file}' \
    --datadir /var/lib/qfc --force \
    --start-cmd 'systemctl start qfc-node' \
    --peer-rpc-url http://peer-b:8545

  # Just check an archive is restorable (CI / backup verification):
  restore.sh --verify-only /backups/qfc-snapshot-1200-1765432100.tar.gz

EXIT CODES:
  0 success; 1 usage/verify/place failure; 2 validation timed out (DB was
  placed — the node may still be syncing; investigate before re-running).
EOF
}

log()  { printf '[restore] %s\n' "$*" >&2; }
fail() { printf '[restore] ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- arguments
ARCHIVE="${ARCHIVE:-}"
FETCH_CMD="${RESTORE_FETCH_CMD:-}"
DATADIR="${QFC_DATA_DIR:-./data}"
PREFIX="${QFC_SNAPSHOT_PREFIX:-qfc-snapshot}"
FORCE=0
START_CMD=""
RPC_URL="http://127.0.0.1:8545"
PEER_RPC_URL=""
VALIDATE=0
VALIDATE_TIMEOUT=180
VERIFY_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --archive)          ARCHIVE="${2:?--archive needs a value}"; shift 2 ;;
    --fetch-cmd)        FETCH_CMD="${2:?--fetch-cmd needs a value}"; shift 2 ;;
    --datadir)          DATADIR="${2:?--datadir needs a value}"; shift 2 ;;
    --prefix)           PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --force)            FORCE=1; shift ;;
    --start-cmd)        START_CMD="${2:?--start-cmd needs a value}"; shift 2 ;;
    --rpc-url)          RPC_URL="${2:?--rpc-url needs a value}"; shift 2 ;;
    --peer-rpc-url)     PEER_RPC_URL="${2:?--peer-rpc-url needs a value}"; shift 2 ;;
    --validate)         VALIDATE=1; shift ;;
    --validate-timeout) VALIDATE_TIMEOUT="${2:?--validate-timeout needs a value}"; shift 2 ;;
    --verify-only)      VERIFY_ONLY=1; shift ;;
    -h|--help)          usage; exit 0 ;;
    -*)                 fail "unknown option: $1 (see --help)" ;;
    *)                  [ -z "$ARCHIVE" ] || fail "multiple archives given: '$ARCHIVE' and '$1'"
                        ARCHIVE="$1"; shift ;;
  esac
done

if [ -n "$START_CMD" ]; then VALIDATE=1; fi
command -v tar >/dev/null  || fail "tar not found"
command -v curl >/dev/null || fail "curl not found (needed for validation)"
if ! command -v jq >/dev/null; then
  log "WARNING: jq not found — manifest verification will be degraded (grep-based)"
fi

# ---------------------------------------------------------------- workspace
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/qfc-restore.XXXXXX")"
# shellcheck disable=SC2329  # invoked via trap
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

# -------------------------------------------------------------------- fetch
if [ -n "$FETCH_CMD" ]; then
  [ -z "$ARCHIVE" ] || fail "give either an archive path or --fetch-cmd, not both"
  ARCHIVE="$WORKDIR/fetched-snapshot.tar.gz"
  # Mirror the node's upload-cmd convention: {file} -> single-quoted local path.
  quoted="'$(printf '%s' "$ARCHIVE" | sed "s/'/'\\\\''/g")'"
  case "$FETCH_CMD" in
    *"{file}"*) cmd="${FETCH_CMD//\{file\}/$quoted}" ;;
    *)          cmd="$FETCH_CMD $quoted" ;;
  esac
  log "Fetching archive: $cmd"
  sh -c "$cmd" || fail "fetch command failed"
  [ -s "$ARCHIVE" ] || fail "fetch command did not produce $ARCHIVE"
fi

[ -n "$ARCHIVE" ] || { usage >&2; fail "no archive given (path or --fetch-cmd)"; }

# Directory given: pick the newest archive matching the prefix.
if [ -d "$ARCHIVE" ]; then
  newest=""
  for f in "$ARCHIVE/$PREFIX"-*.tar.gz; do
    [ -f "$f" ] || continue
    if [ -z "$newest" ] || [ "$f" -nt "$newest" ]; then newest="$f"; fi
  done
  [ -n "$newest" ] || fail "no $PREFIX-*.tar.gz archives in $ARCHIVE"
  log "Newest archive in directory: $newest"
  ARCHIVE="$newest"
fi
[ -f "$ARCHIVE" ] || fail "archive not found: $ARCHIVE"

# ------------------------------------------------------------------- verify
log "Verifying archive integrity: $ARCHIVE"
tar -tzf "$ARCHIVE" >"$WORKDIR/toc" || fail "archive is corrupt (tar/gzip integrity check failed)"

# Exactly one top-level directory, named like the archive stem.
roots="$(sed -e 's#^\./##' -e 's#/.*##' "$WORKDIR/toc" | sort -u)"
[ "$(printf '%s\n' "$roots" | wc -l | tr -d ' ')" = 1 ] \
  || fail "archive must contain exactly one top-level directory, found: $(printf '%s ' "$roots")"
ROOT="$roots"
grep -qx "$ROOT/qfc-snapshot-manifest.json" "$WORKDIR/toc" \
  || fail "archive has no qfc-snapshot-manifest.json under $ROOT/ — not a QFC snapshot"

STAGE="$WORKDIR/stage"
mkdir -p "$STAGE"
log "Extracting to staging area..."
tar -xzf "$ARCHIVE" -C "$STAGE" || fail "extraction failed"
SNAPDIR="$STAGE/$ROOT"
MANIFEST="$SNAPDIR/qfc-snapshot-manifest.json"
[ -f "$MANIFEST" ] || fail "manifest missing after extraction"

if command -v jq >/dev/null; then
  jq -e . "$MANIFEST" >/dev/null || fail "manifest is not valid JSON"
  fmt="$(jq -r '.format_version' "$MANIFEST")"
  [ "$fmt" = "1" ] || fail "unsupported snapshot format_version: $fmt (this script understands 1)"
  height="$(jq -r '.block_height // "none"' "$MANIFEST")"
  created="$(jq -r '.created_at_unix' "$MANIFEST")"
  cf_count="$(jq -r '.column_families | length' "$MANIFEST")"
  jq -e '.column_families | index("metadata") and index("checkpoints")' "$MANIFEST" >/dev/null \
    || fail "manifest column_families missing 'metadata'/'checkpoints' — incompatible snapshot"
else
  grep -q '"format_version": *1' "$MANIFEST" || fail "unsupported or unreadable format_version"
  height="$(sed -n 's/.*"block_height": *\([0-9]*\).*/\1/p' "$MANIFEST" | head -1)"
  created="$(sed -n 's/.*"created_at_unix": *\([0-9]*\).*/\1/p' "$MANIFEST" | head -1)"
  cf_count="?"
fi
[ -f "$SNAPDIR/CURRENT" ] || fail "snapshot has no RocksDB CURRENT file — not a database"

age=$(( $(date +%s) - created ))
log "Snapshot OK: height=${height:-none} created_at=$created (age ${age}s) column_families=$cf_count"
if [ "$age" -gt $((24 * 3600)) ]; then
  log "WARNING: snapshot is older than 24h — expect a long catch-up sync after restore"
fi

if [ "$VERIFY_ONLY" = 1 ]; then
  log "--verify-only: archive is restorable, exiting."
  exit 0
fi

# -------------------------------------------------------------------- place
DB_DIR="$DATADIR/db"
mkdir -p "$DATADIR"
if [ -e "$DB_DIR" ]; then
  if [ "$FORCE" != 1 ]; then
    fail "$DB_DIR already exists — stop the node and re-run with --force (the old DB is kept as db.bak.<ts>)"
  fi
  bak="$DB_DIR.bak.$(date +%s)"
  log "Preserving existing database as $bak"
  mv "$DB_DIR" "$bak"
fi

# Stage inside the datadir so the final rename is atomic (same filesystem).
PLACE_TMP="$DATADIR/.restore-incoming.$$"
rm -rf "$PLACE_TMP"
mv "$SNAPDIR" "$PLACE_TMP" 2>/dev/null || { cp -R "$SNAPDIR" "$PLACE_TMP"; }
mv "$PLACE_TMP" "$DB_DIR"
log "Database placed at $DB_DIR (height ${height:-none})"

# ------------------------------------------------------------------ restart
if [ -n "$START_CMD" ]; then
  log "Starting node: $START_CMD"
  sh -c "$START_CMD" || fail "start command failed"
else
  log "No --start-cmd given. Start the node yourself, e.g.:"
  log "  qfc-node --datadir '$DATADIR' <your usual flags>"
fi

# ----------------------------------------------------------------- validate
if [ "$VALIDATE" != 1 ]; then
  log "Restore complete (validation skipped — pass --validate or --start-cmd to poll RPC)."
  exit 0
fi

rpc_block_number() { # $1 = url; prints decimal height or nothing
  local hex
  hex="$(curl -sf --max-time 5 "$1" -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    | sed -n 's/.*"result" *: *"0x\([0-9a-fA-F]*\)".*/\1/p' || true)"
  if [ -n "$hex" ]; then printf '%d\n' "0x$hex"; fi
}

log "Validating: polling $RPC_URL for an advancing block number (timeout ${VALIDATE_TIMEOUT}s)..."
deadline=$(( $(date +%s) + VALIDATE_TIMEOUT ))
first_seen=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  bn="$(rpc_block_number "$RPC_URL")"
  if [ -n "$bn" ]; then
    if [ -z "$first_seen" ]; then
      first_seen="$bn"
      log "RPC is up, height $bn — waiting for it to advance..."
    elif [ "$bn" -gt "$first_seen" ]; then
      log "VALIDATED: block number advancing ($first_seen -> $bn)"
      if [ -n "$PEER_RPC_URL" ]; then
        peer_bn="$(rpc_block_number "$PEER_RPC_URL")"
        if [ -n "$peer_bn" ]; then
          log "Peer $PEER_RPC_URL is at height $peer_bn (local lag: $((peer_bn - bn)) blocks)"
          if [ $((peer_bn - bn)) -gt 50 ]; then
            log "NOTE: lag >50 blocks — watch qfc_sync_lag_blocks until caught up"
          fi
        else
          log "WARNING: peer RPC $PEER_RPC_URL unreachable — skipping comparison"
        fi
      fi
      log "Restore complete."
      exit 0
    fi
  fi
  sleep 1
done
log "ERROR: block number did not advance within ${VALIDATE_TIMEOUT}s (last seen: ${first_seen:-unreachable})"
log "The database WAS placed; check node logs before re-running."
exit 2
