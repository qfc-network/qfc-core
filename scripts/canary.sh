#!/usr/bin/env bash
# canary.sh — canary rollout driver for qfc-node releases (T7).
#
# State machine:
#
#   start ──> watching ──(N clean epochs)──> eligible ──promote──> promoted
#                │
#                └──(sustained guardrail violations)──> rolled_back
#                                                       (or rollback_failed)
#
# Deployment-agnostic: every mutation (deploy / promote / rollback) runs an
# operator-supplied shell command with {version} and {node} substituted, so it
# works with docker compose, systemd, k8s, or anything else. All observations
# come from Prometheus (--prom-url / PROM_URL) or, as a fallback that needs no
# Prometheus at all, directly from the canary node's /metrics text endpoint
# (--metrics-url). State lives in a JSON file so every invocation is
# resumable and idempotent, and every mutation is recorded in an audit trail.
#
# Guardrails (see docs/CANARY.md for the catalog and rationale):
#   block_age      head block age within bounds (production not stalled)
#   height_advance head advancing between polls
#   height_lag     canary not falling behind fleet max height (Prometheus mode)
#   rpc_errors     server-side RPC error ratio: absolute + vs fleet ratio
#   rpc_p99        RPC p99 latency: absolute + vs fleet ratio
#   sync           sync lag bounded, not stuck syncing
#   storage        write_stopped == 0, pending compaction bounded
#
# Exit codes: 0 ok / promoted / eligible; 1 usage or runtime error;
# 2 rollback triggered (dry-run: would have rolled back); 3 rollback command
# FAILED (manual intervention required); 4 watch hit --max-duration-secs
# without a verdict.
# SC2016: single-quoted '$var' strings in this script are jq filters, where
# $var is a jq variable bound via --arg/--argjson — never shell expansion.
# shellcheck disable=SC2016
set -euo pipefail

usage() {
  cat <<'EOF'
canary.sh — staged canary rollout with SLO guardrails and auto-rollback (T7)

USAGE:
  canary.sh <start|watch|promote|rollback|status> [OPTIONS]

SUBCOMMANDS:
  start      Record baseline, deploy --version to the canary node via
             --deploy-cmd, initialize the state file. With --wait, runs
             watch immediately afterwards.
  watch      Poll guardrails until the canary is promote-eligible (N clean
             epochs) or sustained violations trigger auto-rollback.
  promote    Run --promote-cmd, only if the state says eligible
             (--force overrides, loudly).
  rollback   Run --rollback-cmd with {version} = recorded baseline. Manual
             trigger; watch calls the same path automatically.
  status     Human-readable state + last guardrail evaluation.

TARGET / DEPLOYMENT (mutations; every command runs via `sh -c`):
  --version V            New version to canary (required for start).
  --node NAME            Canary node identifier, substituted for {node}
                         (required for start).
  --current-version V    Baseline version. Auto-detected from the canary's
                         qfc_nodeInfo RPC (--rpc-url) or the
                         qfc_node_info{version=...} metric when omitted.
  --deploy-cmd CMD       Deploys {version} to {node}.       [env: CANARY_DEPLOY_CMD]
  --promote-cmd CMD      Rolls {version} out to the fleet.  [env: CANARY_PROMOTE_CMD]
  --rollback-cmd CMD     Redeploys {version} (= baseline) to {node}.
                                                            [env: CANARY_ROLLBACK_CMD]

OBSERVATION (at least one of --prom-url / --metrics-url is required for watch):
  --prom-url URL         Prometheus base URL (e.g. http://prom:9090).
                         Enables fleet-relative checks.     [env: PROM_URL]
  --canary-instance I    The canary's `instance` label in Prometheus
                         (e.g. node-b:6060). Required with --prom-url.
  --metrics-url URL      Canary /metrics endpoint, e.g. http://node-b:6060/metrics.
                         Fallback when Prometheus is unavailable: absolute
                         thresholds only, rates computed between polls.
  --rpc-url URL          Canary JSON-RPC endpoint, for baseline version
                         auto-detection via qfc_nodeInfo.

GUARDRAIL THRESHOLDS (defaults match docs/observability/alert-rules.yaml):
  --max-block-age S      Max head-block age in seconds (default 30).
  --max-height-lag N     Max blocks behind fleet max height (default 10).
  --max-sync-lag N       Max qfc_sync_lag_blocks (default 50).
  --max-rpc-error-rate R Max server-side RPC error ratio (default 0.01).
  --max-rpc-p99 S        Max RPC p99 latency in seconds (default 0.5).
  --fleet-ratio R        Max canary/fleet ratio for error rate and p99
                         (default 1.5; Prometheus mode only).
  --max-compaction-gb N  Max pending compaction debt in GiB (default 64).

ROLLOUT POLICY:
  --clean-epochs N       Consecutive clean epochs before promote-eligible
                         (default 3).
  --epoch-blocks N       Blocks per epoch window (default 3 = dev-chain
                         qfc_types::BLOCKS_PER_EPOCH; set to your deploy's
                         epoch length).
  --clean-window-secs S  Time-based windows instead of block-based ones.
  --poll-secs S          Guardrail poll interval (default 15).
  --violations-to-rollback N
                         Consecutive dirty polls before auto-rollback
                         (default 3; transient blips don't kill a canary).
  --max-duration-secs S  Give up watching after S seconds (exit 4). 0 = no
                         limit (default).

GENERAL:
  --state-file PATH      State/audit JSON (default .canary-state.json).
  --dry-run              Evaluate everything, log what WOULD run, but never
                         execute a mutation command.
  --wait                 (start) run watch right after deploying.
  --force                promote without eligibility / overwrite live state.
  -h, --help             This help.

WORKED EXAMPLE (ghcr image tag + docker compose over SSH, the qfc-testnet
fleet layout; any deployment works as long as the three commands do):

  export PROM_URL=http://prom.internal:9090
  DEPLOY='ssh {node} "cd /opt/qfc && QFC_TAG={version} docker compose up -d --pull always qfc-node"'
  ROLLBACK="$DEPLOY"
  PROMOTE='for h in vps-a vps-c vps-d; do ssh $h "cd /opt/qfc && QFC_TAG={version} docker compose up -d --pull always qfc-node"; done'

  # 1. deploy v1.4.0 to vps-b and watch it for 3 clean epochs
  scripts/canary.sh start --version v1.4.0 --node vps-b \
      --canary-instance vps-b:6060 --rpc-url http://vps-b:8545 \
      --deploy-cmd "$DEPLOY" --rollback-cmd "$ROLLBACK" --promote-cmd "$PROMOTE" \
      --epoch-blocks 100 --clean-epochs 3 --wait

  # 2. (state now says eligible) roll the fleet
  scripts/canary.sh promote --promote-cmd "$PROMOTE"

  # No Prometheus? Point straight at the canary's exporter:
  scripts/canary.sh watch --metrics-url http://vps-b:6060/metrics \
      --rollback-cmd "$ROLLBACK"

EXIT CODES:
  0 success/eligible/promoted   2 rollback triggered (dry-run: would have)
  1 usage or runtime error      3 ROLLBACK COMMAND FAILED — intervene now
                                4 --max-duration-secs hit, no verdict
EOF
}

# ---------------------------------------------------------------- defaults
STATE_FILE=".canary-state.json"
PROM_URL="${PROM_URL:-}"
METRICS_URL=""
RPC_URL=""
CANARY_INSTANCE=""
NEW_VERSION=""
CANARY_NODE=""
CURRENT_VERSION=""
DEPLOY_CMD="${CANARY_DEPLOY_CMD:-}"
PROMOTE_CMD="${CANARY_PROMOTE_CMD:-}"
ROLLBACK_CMD="${CANARY_ROLLBACK_CMD:-}"
MAX_BLOCK_AGE=30
MAX_HEIGHT_LAG=10
MAX_SYNC_LAG=50
MAX_RPC_ERROR_RATE=0.01
MAX_RPC_P99=0.5
FLEET_RATIO=1.5
MAX_COMPACTION_GB=64
CLEAN_EPOCHS=3
EPOCH_BLOCKS=3
CLEAN_WINDOW_SECS=0
POLL_SECS=15
VIOLATIONS_TO_ROLLBACK=3
MAX_DURATION_SECS=0
DRY_RUN=0
WAIT_AFTER_START=0
FORCE=0

# Floors below which ratio-vs-fleet checks don't fire (absolute noise gates:
# a canary at 0.2% errors vs a fleet at 0.1% is 2x but not a regression).
ERROR_RATE_FLOOR=0.005
P99_FLOOR=0.1

# Excluded caller-side JSON-RPC error codes (same set as alert-rules.yaml).
CLIENT_ERROR_CODES='-32700|-32600|-32601|-32602'

log()  { printf '%s [canary] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }
warn() { printf '%s [canary] WARNING: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; }
die()  { printf '%s [canary] ERROR: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "required tool '$1' not found"; }

# ---------------------------------------------------------------- arg parsing
[ $# -ge 1 ] || { usage; exit 1; }
case "$1" in
  -h|--help) usage; exit 0 ;;
esac
SUBCMD="$1"; shift
case "$SUBCMD" in
  start|watch|promote|rollback|status) ;;
  *) usage; die "unknown subcommand '$SUBCMD'" ;;
esac

# Policy flags passed explicitly on this invocation override the policy
# frozen in the state file (so `watch --clean-epochs 5` does what it says).
SET_CLEAN_EPOCHS=0
SET_EPOCH_BLOCKS=0
SET_CLEAN_WINDOW=0
SET_VIOLATIONS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version)            NEW_VERSION="$2"; shift 2 ;;
    --node)               CANARY_NODE="$2"; shift 2 ;;
    --current-version)    CURRENT_VERSION="$2"; shift 2 ;;
    --deploy-cmd)         DEPLOY_CMD="$2"; shift 2 ;;
    --promote-cmd)        PROMOTE_CMD="$2"; shift 2 ;;
    --rollback-cmd)       ROLLBACK_CMD="$2"; shift 2 ;;
    --prom-url)           PROM_URL="$2"; shift 2 ;;
    --canary-instance)    CANARY_INSTANCE="$2"; shift 2 ;;
    --metrics-url)        METRICS_URL="$2"; shift 2 ;;
    --rpc-url)            RPC_URL="$2"; shift 2 ;;
    --max-block-age)      MAX_BLOCK_AGE="$2"; shift 2 ;;
    --max-height-lag)     MAX_HEIGHT_LAG="$2"; shift 2 ;;
    --max-sync-lag)       MAX_SYNC_LAG="$2"; shift 2 ;;
    --max-rpc-error-rate) MAX_RPC_ERROR_RATE="$2"; shift 2 ;;
    --max-rpc-p99)        MAX_RPC_P99="$2"; shift 2 ;;
    --fleet-ratio)        FLEET_RATIO="$2"; shift 2 ;;
    --max-compaction-gb)  MAX_COMPACTION_GB="$2"; shift 2 ;;
    --clean-epochs)       CLEAN_EPOCHS="$2"; SET_CLEAN_EPOCHS=1; shift 2 ;;
    --epoch-blocks)       EPOCH_BLOCKS="$2"; SET_EPOCH_BLOCKS=1; shift 2 ;;
    --clean-window-secs)  CLEAN_WINDOW_SECS="$2"; SET_CLEAN_WINDOW=1; shift 2 ;;
    --poll-secs)          POLL_SECS="$2"; shift 2 ;;
    --violations-to-rollback) VIOLATIONS_TO_ROLLBACK="$2"; SET_VIOLATIONS=1; shift 2 ;;
    --max-duration-secs)  MAX_DURATION_SECS="$2"; shift 2 ;;
    --state-file)         STATE_FILE="$2"; shift 2 ;;
    --dry-run)            DRY_RUN=1; shift ;;
    --wait)               WAIT_AFTER_START=1; shift ;;
    --force)              FORCE=1; shift ;;
    -h|--help)            usage; exit 0 ;;
    *) die "unknown option '$1' (see --help)" ;;
  esac
done

need jq
need curl
need awk

# ---------------------------------------------------------------- state file
state_get() { jq -r "$1" "$STATE_FILE"; }

# state_update <jq-filter> [--argjson/--arg pairs...]
state_update() {
  local filter="$1"; shift
  local tmp
  tmp="$(mktemp "${STATE_FILE}.XXXXXX")"
  jq "$@" "$filter" "$STATE_FILE" > "$tmp"
  mv "$tmp" "$STATE_FILE"
}

audit() { # audit <action> <detail> [cmd] [exit_code]
  local action="$1" detail="$2" cmd="${3:-}" code="${4:-}"
  state_update '.audit += [{ts: $ts, action: $a, detail: $d, cmd: $c, exit_code: $e, dry_run: ($dr == "1")}]' \
    --arg ts "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" --arg a "$action" --arg d "$detail" \
    --arg c "$cmd" --arg e "$code" --arg dr "$DRY_RUN"
}

require_state() {
  [ -f "$STATE_FILE" ] || die "no state file at $STATE_FILE — run 'start' first (or pass --state-file)"
}

# ---------------------------------------------------------------- mutations
# run_mutation <label> <cmd-template> <version> <node>  — the ONLY place that
# executes operator-supplied commands. {version}/{node} substituted, sh -c,
# always audited. Honors --dry-run.
run_mutation() {
  local label="$1" template="$2" version="$3" node="$4"
  [ -n "$template" ] || die "no command configured for '$label' (--${label}-cmd)"
  local cmd="${template//\{version\}/$version}"
  cmd="${cmd//\{node\}/$node}"
  if [ "$DRY_RUN" = 1 ]; then
    log "DRY-RUN: would run $label: $cmd"
    audit "$label" "dry-run, not executed" "$cmd" "skipped"
    return 0
  fi
  log "running $label: $cmd"
  local rc=0
  sh -c "$cmd" || rc=$?
  audit "$label" "executed" "$cmd" "$rc"
  if [ "$rc" -ne 0 ]; then
    warn "$label command exited $rc"
    return "$rc"
  fi
  log "$label OK"
}

# ---------------------------------------------------------------- observation
# Two backends. Prometheus: instant queries, fleet-relative checks available.
# /metrics fallback: scrape the canary's exporter, compute rates from deltas
# between polls (previous counters cached in the state file).

prom_query() { # prom_query <promql> -> scalar or "" when no data
  local q="$1" out
  out="$(curl -fsS --max-time 10 --get "$PROM_URL/api/v1/query" --data-urlencode "query=$q" 2>/dev/null)" || return 1
  jq -r 'if .status=="success" and (.data.result|length) > 0 then .data.result[0].value[1] else "" end' <<<"$out"
}

# scrape_metrics — fetch the /metrics text into $SCRAPE (global).
SCRAPE=""
scrape_metrics() {
  SCRAPE="$(curl -fs --max-time 10 "$METRICS_URL" 2>/dev/null)" || return 1
}

m_gauge() { # m_gauge <name> [default] — unlabeled sample from $SCRAPE
  local v
  v="$(awk -v n="$1" '$1 == n {print $2; exit}' <<<"$SCRAPE")"
  printf '%s' "${v:-${2:-}}"
}

m_sum() { # m_sum <name-prefix-regex> — sum samples whose name matches (labels allowed)
  awk -v re="^$1(\\{|[[:space:]])" 'BEGIN{s=0} $0 ~ re && $0 !~ /^#/ {s+=$NF} END{printf "%.6f", s}' <<<"$SCRAPE"
}

# Sum of server-side rpc errors from $SCRAPE (client error codes excluded).
m_rpc_errors() {
  awk -v codes="$CLIENT_ERROR_CODES" '
    BEGIN{s=0; n=split(codes, ex, "|")}
    /^qfc_rpc_errors_total\{/ && !/^#/ {
      skip=0
      for (i=1; i<=n; i++) if (index($0, "code=\"" ex[i] "\"")) skip=1
      if (!skip) s+=$NF
    }
    END{printf "%.6f", s}' <<<"$SCRAPE"
}

# Histogram buckets summed across methods: JSON {le: count} on stdout.
m_buckets_json() {
  awk '
    /^qfc_rpc_request_duration_seconds_bucket\{/ && !/^#/ {
      if (match($0, /le="[^"]*"/)) {
        le = substr($0, RSTART+4, RLENGTH-5)
        sum[le] += $NF
      }
    }
    END{
      printf "{"
      first=1
      for (le in sum) {
        if (!first) printf ","
        printf "\"%s\":%.6f", le, sum[le]
        first=0
      }
      printf "}"
    }' <<<"$SCRAPE"
}

# p99 from two cumulative bucket snapshots (delta histogram quantile).
# usage: bucket_p99 <prev_json> <cur_json> -> seconds (or "" if no traffic)
bucket_p99() {
  jq -rn --argjson prev "$1" --argjson cur "$2" '
    [ $cur | keys[] ] as $les
    | [ $les[] | {le: ., d: (($cur[.] // 0) - ($prev[.] // 0))} ]
    | map(select(.d >= 0))
    | sort_by(if .le == "+Inf" then 1e308 else (.le | tonumber) end)
    | . as $b
    | ($b | map(select(.le == "+Inf")) | first | .d // 0) as $total
    | if $total <= 0 then ""
      else ( [ $b[] | select(.d >= $total * 0.99) ] | first
             | if .le == "+Inf" then "inf" else .le end )
      end'
}

float_cmp() { # float_cmp <a> <op> <b> -> exit 0 if true
  awk -v a="$1" -v b="$3" -v op="$2" 'BEGIN{
    if (op == ">")  exit !(a >  b)
    if (op == ">=") exit !(a >= b)
    if (op == "<")  exit !(a <  b)
    if (op == "<=") exit !(a <= b)
    exit 1
  }'
}

# ---------------------------------------------------------------- version detection
detect_version() {
  local v=""
  if [ -n "$RPC_URL" ]; then
    v="$(curl -fsS --max-time 10 -X POST -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","method":"qfc_nodeInfo","params":[],"id":1}' "$RPC_URL" 2>/dev/null \
      | jq -r '.result.version // empty')" || v=""
  fi
  if [ -z "$v" ] && [ -n "$METRICS_URL" ] && scrape_metrics; then
    v="$(awk '/^qfc_node_info\{/ {if (match($0, /version="[^"]*"/)) {print substr($0, RSTART+9, RLENGTH-10); exit}}' <<<"$SCRAPE")"
  fi
  if [ -z "$v" ] && [ -n "$PROM_URL" ] && [ -n "$CANARY_INSTANCE" ]; then
    local out
    out="$(curl -fsS --max-time 10 --get "$PROM_URL/api/v1/query" \
      --data-urlencode "query=qfc_node_info{instance=\"$CANARY_INSTANCE\"}" 2>/dev/null)" || out=""
    [ -n "$out" ] && v="$(jq -r '.data.result[0].metric.version // empty' <<<"$out")"
  fi
  printf '%s' "$v"
}

# ---------------------------------------------------------------- guardrails
# evaluate_guardrails: writes .last_eval into state, prints one line per
# guardrail, returns the number of violations via global EVAL_VIOLATIONS.
EVAL_VIOLATIONS=0
EVAL_JSON="[]"

eval_add() { # eval_add <name> <ok|VIOLATION|skipped> <detail>
  local name="$1" status="$2" detail="$3"
  EVAL_JSON="$(jq -c --arg n "$name" --arg s "$status" --arg d "$detail" \
    '. += [{guardrail: $n, status: $s, detail: $d}]' <<<"$EVAL_JSON")"
  if [ "$status" = "VIOLATION" ]; then
    EVAL_VIOLATIONS=$((EVAL_VIOLATIONS + 1))
    warn "guardrail $name: $detail"
  else
    log "guardrail $name: $status ($detail)"
  fi
}

evaluate_guardrails_prom() {
  local i="$CANARY_INSTANCE" v

  # (a) block production: head age + height vs fleet
  v="$(prom_query "qfc_time_since_last_block_seconds{instance=\"$i\"}")" || v=""
  if [ -z "$v" ]; then
    eval_add block_age VIOLATION "no data from Prometheus for instance $i (scrape down?)"
  elif float_cmp "$v" ">" "$MAX_BLOCK_AGE"; then
    eval_add block_age VIOLATION "head block ${v}s old > ${MAX_BLOCK_AGE}s"
  else
    eval_add block_age ok "head block ${v}s old"
  fi

  local h prev_h
  h="$(prom_query "qfc_block_height{instance=\"$i\"}")" || h=""
  prev_h="$(state_get '.poll_cache.prev_height // empty')"
  check_height_advance "$h" "$prev_h"
  if [ -n "$h" ]; then
    local fleet_max lag
    fleet_max="$(prom_query "max(qfc_block_height{instance!=\"$i\"})")" || fleet_max=""
    if [ -n "$fleet_max" ]; then
      lag="$(awk -v a="$fleet_max" -v b="$h" 'BEGIN{d=a-b; print (d>0)?d:0}')"
      if float_cmp "$lag" ">" "$MAX_HEIGHT_LAG"; then
        eval_add height_lag VIOLATION "canary $lag blocks behind fleet max ($fleet_max vs $h) > $MAX_HEIGHT_LAG"
      else
        eval_add height_lag ok "$lag blocks behind fleet max"
      fi
    else
      eval_add height_lag skipped "no non-canary qfc_block_height series (single-node deploy?)"
    fi
  fi

  # (b) RPC error ratio + p99, absolute and vs fleet
  local err_q="sum(rate(qfc_rpc_errors_total{instance=\"$i\",code!~\"$CLIENT_ERROR_CODES\"}[5m])) / clamp_min(sum(rate(qfc_rpc_requests_total{instance=\"$i\"}[5m])), 1e-10)"
  local err fleet_err
  err="$(prom_query "$err_q")" || err=""
  check_rpc_error_abs "${err:-0}"
  fleet_err="$(prom_query "sum(rate(qfc_rpc_errors_total{instance!=\"$i\",code!~\"$CLIENT_ERROR_CODES\"}[5m])) / clamp_min(sum(rate(qfc_rpc_requests_total{instance!=\"$i\"}[5m])), 1e-10)")" || fleet_err=""
  check_fleet_ratio rpc_errors_vs_fleet "${err:-}" "${fleet_err:-}" "$ERROR_RATE_FLOOR"

  local p99 fleet_p99
  p99="$(prom_query "histogram_quantile(0.99, sum by (le) (rate(qfc_rpc_request_duration_seconds_bucket{instance=\"$i\"}[5m])))")" || p99=""
  [ "$p99" = "NaN" ] && p99=""
  check_rpc_p99_abs "$p99"
  fleet_p99="$(prom_query "histogram_quantile(0.99, sum by (le) (rate(qfc_rpc_request_duration_seconds_bucket{instance!=\"$i\"}[5m])))")" || fleet_p99=""
  [ "$fleet_p99" = "NaN" ] && fleet_p99=""
  check_fleet_ratio rpc_p99_vs_fleet "${p99:-}" "${fleet_p99:-}" "$P99_FLOOR"

  # (c) sync health
  local lag_v sync_v
  lag_v="$(prom_query "qfc_sync_lag_blocks{instance=\"$i\"}")" || lag_v=""
  sync_v="$(prom_query "qfc_sync_syncing{instance=\"$i\"}")" || sync_v=""
  check_sync "${lag_v:-}" "${sync_v:-}"

  # (d) storage health
  local ws comp
  ws="$(prom_query "qfc_rocksdb_write_stopped{instance=\"$i\"}")" || ws=""
  comp="$(prom_query "sum(qfc_rocksdb_pending_compaction_bytes{instance=\"$i\"})")" || comp=""
  check_storage "${ws:-}" "${comp:-}"

  # cache height for next poll
  state_update '.poll_cache.prev_height = $h' --arg h "${h:-}"
}

evaluate_guardrails_metrics() {
  if ! scrape_metrics; then
    eval_add scrape VIOLATION "cannot fetch $METRICS_URL — canary exporter down"
    return
  fi

  # (a) block production
  local age h prev_h
  age="$(m_gauge qfc_time_since_last_block_seconds)"
  if [ -z "$age" ]; then
    eval_add block_age VIOLATION "qfc_time_since_last_block_seconds absent from $METRICS_URL"
  elif float_cmp "$age" ">" "$MAX_BLOCK_AGE"; then
    eval_add block_age VIOLATION "head block ${age}s old > ${MAX_BLOCK_AGE}s"
  else
    eval_add block_age ok "head block ${age}s old"
  fi
  h="$(m_gauge qfc_block_height)"
  prev_h="$(state_get '.poll_cache.prev_height // empty')"
  check_height_advance "$h" "$prev_h"
  eval_add height_lag skipped "fleet height comparison needs --prom-url"

  # (b) RPC: delta-based rates between polls
  local req err prev_req prev_err
  req="$(m_sum qfc_rpc_requests_total)"
  err="$(m_rpc_errors)"
  prev_req="$(state_get '.poll_cache.prev_rpc_requests // empty')"
  prev_err="$(state_get '.poll_cache.prev_rpc_errors // empty')"
  if [ -z "$prev_req" ]; then
    eval_add rpc_errors skipped "first poll, no previous counters yet"
  else
    local dreq derr rate
    dreq="$(awk -v a="$req" -v b="$prev_req" 'BEGIN{d=a-b; print (d>0)?d:0}')"
    derr="$(awk -v a="$err" -v b="$prev_err" 'BEGIN{d=a-b; print (d>0)?d:0}')"
    if float_cmp "$dreq" "<=" "0"; then
      eval_add rpc_errors ok "no RPC traffic since last poll"
    else
      rate="$(awk -v e="$derr" -v r="$dreq" 'BEGIN{printf "%.6f", e/r}')"
      check_rpc_error_abs "$rate"
    fi
  fi

  local buckets prev_buckets p99
  buckets="$(m_buckets_json)"
  prev_buckets="$(state_get '.poll_cache.prev_buckets // empty')"
  if [ -z "$prev_buckets" ]; then
    eval_add rpc_p99 skipped "first poll, no previous histogram yet"
  else
    p99="$(bucket_p99 "$prev_buckets" "$buckets")"
    if [ -z "$p99" ]; then
      eval_add rpc_p99 ok "no RPC traffic since last poll"
    elif [ "$p99" = "inf" ]; then
      eval_add rpc_p99 VIOLATION "p99 above the largest histogram bucket (>10s)"
    else
      check_rpc_p99_abs "$p99"
    fi
  fi
  eval_add rpc_vs_fleet skipped "fleet-relative checks need --prom-url"

  # (c) sync
  check_sync "$(m_gauge qfc_sync_lag_blocks)" "$(m_gauge qfc_sync_syncing)"

  # (d) storage
  check_storage "$(m_gauge qfc_rocksdb_write_stopped)" "$(m_sum qfc_rocksdb_pending_compaction_bytes)"

  state_update '.poll_cache += {prev_height: $h, prev_rpc_requests: $rq, prev_rpc_errors: $re, prev_buckets: $bk}' \
    --arg h "${h:-}" --arg rq "$req" --arg re "$err" --arg bk "$buckets"
}

# Height must advance within --max-block-age seconds of OBSERVER wall clock
# (decoupled from the poll interval — chains with a block interval longer
# than --poll-secs must not false-positive). Complements block_age: that one
# trusts the node's own head timestamp, this one only trusts observed height,
# so a node with a wedged clock or forged timestamps still trips a guardrail.
check_height_advance() { # <height> <prev_height>
  local h="$1" prev="$2" now adv
  now="$(date +%s)"
  if [ -z "$h" ]; then
    eval_add height_advance VIOLATION "qfc_block_height unavailable"
    return
  fi
  adv="$(state_get '.poll_cache.last_advance_ts // empty')"
  if [ -z "$prev" ] || [ -z "$adv" ] || float_cmp "$h" ">" "$prev"; then
    state_update '.poll_cache.last_advance_ts = ($t | tonumber)' --arg t "$now"
    if [ -z "$prev" ]; then
      eval_add height_advance skipped "first poll, height=$h recorded"
    else
      eval_add height_advance ok "height $prev -> $h"
    fi
    return
  fi
  local stalled=$(( now - adv ))
  if [ "$stalled" -gt "${MAX_BLOCK_AGE%%.*}" ]; then
    eval_add height_advance VIOLATION "height stuck at $h for ${stalled}s > ${MAX_BLOCK_AGE}s"
  else
    eval_add height_advance ok "height $h unchanged for ${stalled}s (within ${MAX_BLOCK_AGE}s)"
  fi
}

check_rpc_error_abs() { # <ratio>
  local rate="$1"
  if float_cmp "$rate" ">" "$MAX_RPC_ERROR_RATE"; then
    eval_add rpc_errors VIOLATION "server-side error ratio $rate > $MAX_RPC_ERROR_RATE"
  else
    eval_add rpc_errors ok "server-side error ratio $rate"
  fi
}

check_rpc_p99_abs() { # <p99-seconds or "">
  local p99="$1"
  if [ -z "$p99" ]; then
    eval_add rpc_p99 ok "no RPC traffic in window"
  elif float_cmp "$p99" ">" "$MAX_RPC_P99"; then
    eval_add rpc_p99 VIOLATION "p99 ${p99}s > ${MAX_RPC_P99}s"
  else
    eval_add rpc_p99 ok "p99 ${p99}s"
  fi
}

check_fleet_ratio() { # <name> <canary-val> <fleet-val> <floor>
  local name="$1" cv="$2" fv="$3" floor="$4"
  if [ -z "$cv" ] || [ -z "$fv" ]; then
    eval_add "$name" skipped "canary or fleet series missing"
    return
  fi
  # only meaningful when the canary value clears the absolute noise floor
  if float_cmp "$cv" "<=" "$floor"; then
    eval_add "$name" ok "canary $cv below noise floor $floor"
    return
  fi
  local limit
  limit="$(awk -v f="$fv" -v r="$FLEET_RATIO" -v fl="$floor" 'BEGIN{l=f*r; if (l<fl) l=fl; printf "%.6f", l}')"
  if float_cmp "$cv" ">" "$limit"; then
    eval_add "$name" VIOLATION "canary $cv > ${FLEET_RATIO}x fleet ($fv)"
  else
    eval_add "$name" ok "canary $cv vs fleet $fv (limit $limit)"
  fi
}

check_sync() { # <lag> <syncing>
  local lag="$1" syncing="$2"
  if [ -z "$lag" ]; then
    eval_add sync skipped "sync metrics absent (no sync provider / no peers)"
    return
  fi
  if float_cmp "$lag" ">" "$MAX_SYNC_LAG"; then
    eval_add sync VIOLATION "sync lag $lag blocks > $MAX_SYNC_LAG"
  elif [ "${syncing:-0}" = "1" ] && float_cmp "$lag" ">" "$MAX_HEIGHT_LAG"; then
    eval_add sync VIOLATION "still syncing with lag $lag (> $MAX_HEIGHT_LAG)"
  else
    eval_add sync ok "lag $lag blocks, syncing=${syncing:-0}"
  fi
}

check_storage() { # <write_stopped> <pending_compaction_bytes>
  local ws="$1" comp="$2"
  if [ -n "$ws" ] && [ "${ws%%.*}" != "0" ]; then
    eval_add storage VIOLATION "qfc_rocksdb_write_stopped=$ws (hard write stall)"
    return
  fi
  if [ -n "$comp" ]; then
    local limit
    limit="$(awk -v g="$MAX_COMPACTION_GB" 'BEGIN{printf "%.0f", g*1024*1024*1024}')"
    if float_cmp "$comp" ">" "$limit"; then
      eval_add storage VIOLATION "pending compaction ${comp}B > ${MAX_COMPACTION_GB}GiB"
      return
    fi
  fi
  eval_add storage ok "write_stopped=${ws:-0}, pending_compaction=${comp:-0}B"
}

evaluate_guardrails() {
  EVAL_VIOLATIONS=0
  EVAL_JSON="[]"
  if [ -n "$PROM_URL" ]; then
    [ -n "$CANARY_INSTANCE" ] || die "--canary-instance is required with --prom-url"
    evaluate_guardrails_prom
  elif [ -n "$METRICS_URL" ]; then
    evaluate_guardrails_metrics
  else
    die "need --prom-url (+ --canary-instance) or --metrics-url to observe the canary"
  fi
  state_update '.last_eval = {ts: $ts, violations: ($v | tonumber), checks: $checks}' \
    --arg ts "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" --arg v "$EVAL_VIOLATIONS" --argjson checks "$EVAL_JSON"
}

# ---------------------------------------------------------------- rollback
do_rollback() { # <reason>
  local reason="$1" baseline node
  baseline="$(state_get '.baseline_version')"
  node="$(state_get '.canary_node')"
  warn "ROLLING BACK canary $node to baseline $baseline — reason: $reason"
  if [ "$DRY_RUN" = 1 ]; then
    run_mutation rollback "$ROLLBACK_CMD" "$baseline" "$node"
    state_update '.phase = "watching"'   # dry-run never mutates the rollout
    warn "DRY-RUN: rollback NOT executed; canary would have been rolled back"
    return 0
  fi
  if run_mutation rollback "$ROLLBACK_CMD" "$baseline" "$node"; then
    state_update '.phase = "rolled_back" | .rolled_back_reason = $r' --arg r "$reason"
    warn "rollback complete; canary $node is back on $baseline"
  else
    state_update '.phase = "rollback_failed" | .rolled_back_reason = $r' --arg r "$reason"
    warn "********************************************************************"
    warn "* ROLLBACK COMMAND FAILED. The canary may be running a bad version *"
    warn "* AND the rollback did not complete. INTERVENE MANUALLY NOW.       *"
    warn "* Not retrying automatically: a failing deploy command must never  *"
    warn "* be retry-looped. See 'status' for the audit trail.               *"
    warn "********************************************************************"
    exit 3
  fi
}

# ---------------------------------------------------------------- subcommands
cmd_start() {
  [ -n "$NEW_VERSION" ] || die "start requires --version"
  [ -n "$CANARY_NODE" ] || die "start requires --node"
  [ -n "$DEPLOY_CMD" ]  || die "start requires --deploy-cmd (or CANARY_DEPLOY_CMD)"

  if [ -f "$STATE_FILE" ]; then
    local phase target
    phase="$(state_get '.phase // empty')" || phase=""
    target="$(state_get '.target_version // empty')" || target=""
    if [ "$target" = "$NEW_VERSION" ] && { [ "$phase" = "watching" ] || [ "$phase" = "eligible" ]; }; then
      log "canary for $NEW_VERSION already $phase (state: $STATE_FILE) — nothing to do"
      [ "$WAIT_AFTER_START" = 1 ] && cmd_watch
      return 0
    fi
    if [ "$FORCE" != 1 ] && [ "$phase" != "promoted" ] && [ "$phase" != "rolled_back" ]; then
      die "state file $STATE_FILE has an active rollout (phase=$phase, target=$target); finish it or pass --force"
    fi
  fi

  local baseline="$CURRENT_VERSION"
  if [ -z "$baseline" ]; then
    log "detecting baseline version (qfc_nodeInfo RPC / qfc_node_info metric)..."
    baseline="$(detect_version)"
    [ -n "$baseline" ] || die "could not auto-detect the running version — pass --current-version"
    log "detected baseline version: $baseline"
  fi
  [ "$baseline" != "$NEW_VERSION" ] || warn "baseline and target version are both '$NEW_VERSION' — canary will be a no-op test"

  jq -n \
    --arg ts "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    --arg node "$CANARY_NODE" --arg base "$baseline" --arg target "$NEW_VERSION" \
    --argjson ce "$CLEAN_EPOCHS" --argjson eb "$EPOCH_BLOCKS" --argjson cw "$CLEAN_WINDOW_SECS" \
    --argjson vr "$VIOLATIONS_TO_ROLLBACK" \
    '{
      schema: 1, phase: "deploying", started_at: $ts,
      canary_node: $node, baseline_version: $base, target_version: $target,
      policy: {clean_epochs_required: $ce, epoch_blocks: $eb,
               clean_window_secs: $cw, violations_to_rollback: $vr},
      clean_epochs: 0, consecutive_violations: 0,
      window: null, poll_cache: {}, last_eval: null, audit: []
    }' > "$STATE_FILE"
  log "state initialized: $STATE_FILE (baseline=$baseline target=$NEW_VERSION canary=$CANARY_NODE)"
  audit start "baseline=$baseline target=$NEW_VERSION node=$CANARY_NODE"

  if run_mutation deploy "$DEPLOY_CMD" "$NEW_VERSION" "$CANARY_NODE"; then
    state_update '.phase = "watching"'
    log "canary deployed; phase -> watching. Run 'watch' (or pass --wait) to evaluate guardrails."
  else
    state_update '.phase = "deploy_failed"'
    die "deploy command failed — canary NOT started (phase=deploy_failed)"
  fi

  [ "$WAIT_AFTER_START" = 1 ] && cmd_watch
  return 0
}

cmd_watch() {
  require_state
  local phase
  phase="$(state_get '.phase')"
  case "$phase" in
    watching) ;;
    eligible) log "canary already promote-eligible; run 'promote'"; return 0 ;;
    deploying|deploy_failed) die "deploy did not complete (phase=$phase) — re-run 'start'" ;;
    promoted) log "already promoted; nothing to watch"; return 0 ;;
    rolled_back) die "canary was rolled back ($(state_get '.rolled_back_reason // "manual"')); start a new rollout" ;;
    rollback_failed) die "previous rollback FAILED — fix the fleet manually before doing anything else" ;;
    *) die "unexpected phase '$phase' in $STATE_FILE" ;;
  esac

  # apply explicit policy flags from this invocation onto the frozen policy
  [ "$SET_CLEAN_EPOCHS" = 1 ] && state_update '.policy.clean_epochs_required = ($v | tonumber)' --arg v "$CLEAN_EPOCHS"
  [ "$SET_EPOCH_BLOCKS" = 1 ] && state_update '.policy.epoch_blocks = ($v | tonumber)' --arg v "$EPOCH_BLOCKS"
  [ "$SET_CLEAN_WINDOW" = 1 ] && state_update '.policy.clean_window_secs = ($v | tonumber)' --arg v "$CLEAN_WINDOW_SECS"
  [ "$SET_VIOLATIONS" = 1 ]   && state_update '.policy.violations_to_rollback = ($v | tonumber)' --arg v "$VIOLATIONS_TO_ROLLBACK"

  local need_clean violations_limit started
  need_clean="$(state_get '.policy.clean_epochs_required')"
  violations_limit="$(state_get '.policy.violations_to_rollback')"
  started="$(date +%s)"
  log "watching canary $(state_get '.canary_node') (target $(state_get '.target_version'))"
  log "policy: $need_clean clean epoch(s) to promote, rollback after $violations_limit consecutive dirty polls, poll every ${POLL_SECS}s"

  while :; do
    evaluate_guardrails

    local consec clean
    if [ "$EVAL_VIOLATIONS" -gt 0 ]; then
      state_update '.consecutive_violations += 1 | .window.violations += 1'
      consec="$(state_get '.consecutive_violations')"
      warn "poll dirty: $EVAL_VIOLATIONS violation(s); consecutive dirty polls: $consec/$violations_limit"
      if [ "$consec" -ge "$violations_limit" ]; then
        do_rollback "$consec consecutive dirty polls (limit $violations_limit); last: $(jq -c '[.[] | select(.status=="VIOLATION") | .guardrail]' <<<"$EVAL_JSON")"
        exit 2
      fi
    else
      state_update '.consecutive_violations = 0'
    fi

    # epoch / window accounting
    maybe_advance_window
    clean="$(state_get '.clean_epochs')"
    log "clean epochs: $clean/$need_clean"
    if [ "$clean" -ge "$need_clean" ]; then
      state_update '.phase = "eligible"'
      audit eligible "$clean clean epochs"
      log "canary is PROMOTE-ELIGIBLE ($clean clean epochs). Run: canary.sh promote"
      return 0
    fi

    if [ "$MAX_DURATION_SECS" -gt 0 ] && [ $(( $(date +%s) - started )) -ge "$MAX_DURATION_SECS" ]; then
      warn "watch hit --max-duration-secs $MAX_DURATION_SECS without a verdict (clean=$clean/$need_clean)"
      exit 4
    fi
    sleep "$POLL_SECS"
  done
}

# Window = one "epoch" for promotion accounting. Block-based windows close
# when the canary head crosses window_start_height + epoch_blocks; time-based
# windows (--clean-window-secs) close on wall clock. A closed window with zero
# violations increments clean_epochs; with violations it resets it to zero.
maybe_advance_window() {
  local now h
  now="$(date +%s)"
  h="$(state_get '.poll_cache.prev_height // empty')"

  if [ "$(state_get '.window.start_ts // empty')" = "" ]; then
    state_update '.window = {start_ts: ($ts | tonumber), start_height: ($h | if . == "" then null else tonumber end), violations: (.window.violations // 0)}' \
      --arg ts "$now" --arg h "${h:-}"
    return 0
  fi

  local closed=0
  local cw
  cw="$(state_get '.policy.clean_window_secs')"
  if [ "$cw" -gt 0 ] 2>/dev/null; then
    local ws
    ws="$(state_get '.window.start_ts')"
    if [ $(( now - ws )) -ge "$cw" ]; then closed=1; fi
  else
    local sh eb
    sh="$(state_get '.window.start_height // empty')"
    eb="$(state_get '.policy.epoch_blocks')"
    if [ -z "$sh" ]; then
      # height was unknown when the window opened; backfill once we have it
      if [ -n "$h" ]; then
        state_update '.window.start_height = ($h | tonumber)' --arg h "$h"
      fi
      return 0
    fi
    if [ -n "$h" ] && float_cmp "$h" ">=" "$((sh + eb))"; then closed=1; fi
  fi

  if [ "$closed" = 1 ]; then
    local wv
    wv="$(state_get '.window.violations')"
    if [ "$wv" -eq 0 ]; then
      state_update '.clean_epochs += 1'
      log "epoch window closed CLEAN"
    else
      state_update '.clean_epochs = 0'
      warn "epoch window closed with $wv violation(s) — clean-epoch counter reset to 0"
    fi
    state_update '.window = {start_ts: ($ts | tonumber), start_height: ($h | if . == "" then null else tonumber end), violations: 0}' \
      --arg ts "$now" --arg h "${h:-}"
  fi
}

cmd_promote() {
  require_state
  local phase target
  phase="$(state_get '.phase')"
  target="$(state_get '.target_version')"
  if [ "$phase" = "promoted" ]; then
    log "already promoted ($target); nothing to do"
    return 0
  fi
  if [ "$phase" != "eligible" ]; then
    if [ "$FORCE" = 1 ]; then
      warn "********************************************************************"
      warn "* --force: promoting WITHOUT eligibility (phase=$phase).           *"
      warn "* The canary has NOT proven $(state_get '.policy.clean_epochs_required') clean epochs. On your head be it. *"
      warn "********************************************************************"
      audit promote-forced "phase was $phase, eligibility bypassed with --force"
    else
      die "refusing to promote: phase is '$phase', not 'eligible'. Run 'watch' until the canary earns it, or --force to override."
    fi
  fi
  if run_mutation promote "$PROMOTE_CMD" "$target" "$(state_get '.canary_node')"; then
    if [ "$DRY_RUN" = 1 ]; then
      log "fleet promotion to $target (dry-run) complete"
    else
      state_update '.phase = "promoted" | .promoted_at = $ts' --arg ts "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
      log "fleet promotion to $target complete"
    fi
  else
    die "promote command failed — fleet may be partially rolled; check the audit trail and your deployment"
  fi
}

cmd_rollback() {
  require_state
  [ -n "$ROLLBACK_CMD" ] || die "rollback requires --rollback-cmd (or CANARY_ROLLBACK_CMD)"
  do_rollback "manual rollback requested"
}

cmd_status() {
  require_state
  local phase
  phase="$(state_get '.phase')"
  echo "canary rollout status  (state: $STATE_FILE)"
  echo "  phase:              $phase"
  echo "  canary node:        $(state_get '.canary_node')"
  echo "  baseline -> target: $(state_get '.baseline_version') -> $(state_get '.target_version')"
  echo "  started:            $(state_get '.started_at')"
  echo "  clean epochs:       $(state_get '.clean_epochs')/$(state_get '.policy.clean_epochs_required')"
  echo "  consecutive dirty:  $(state_get '.consecutive_violations')/$(state_get '.policy.violations_to_rollback')"
  if [ "$(state_get '.last_eval')" != "null" ]; then
    echo "  last evaluation:    $(state_get '.last_eval.ts') (${phase}, $(state_get '.last_eval.violations') violation(s))"
    jq -r '.last_eval.checks[] | "    [\(.status)] \(.guardrail): \(.detail)"' "$STATE_FILE"
  else
    echo "  last evaluation:    never (run 'watch')"
  fi
  echo "  audit trail:        $(jq '.audit | length' "$STATE_FILE") entries (last 5)"
  jq -r '.audit[-5:][] | "    \(.ts) \(.action) exit=\(.exit_code // "-") \(if .dry_run then "[dry-run] " else "" end)\(.cmd // .detail)"' "$STATE_FILE"
}

case "$SUBCMD" in
  start)    cmd_start ;;
  watch)    cmd_watch ;;
  promote)  cmd_promote ;;
  rollback) cmd_rollback ;;
  status)   cmd_status ;;
esac
