#!/usr/bin/env bash
#
# capture.sh — run the QFC SRE T1 eBPF capture suite against a running
# qfc-node container and collect the artifacts into an output directory.
#
# Requires (Linux host): bpftrace, perf, docker, passwordless sudo, and
# kernel BTF (/sys/kernel/btf/vmlinux). Tested on Ubuntu 24.04 / kernel 6.17
# (aarch64, AWS). bcc tools are NOT required — the captures are plain
# bpftrace programs in this directory.
#
# Usage:
#   sudo ./scripts/profiling/capture.sh [CONTAINER] [OUTDIR]
# Defaults: CONTAINER=qfc-node-1  OUTDIR=./t1-ebpf-$(date +%s)
#
# Render the on-CPU flame graph from the folded stacks with either:
#   inferno-flamegraph < OUTDIR/05-oncpu.folded > flame.svg
#   flamegraph.pl       OUTDIR/05-oncpu.folded > flame.svg
set -euo pipefail

CONTAINER="${1:-qfc-node-1}"
OUTDIR="${2:-./t1-ebpf-capture}"
HERE="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$OUTDIR"

PID="$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")"
echo ">> target: $CONTAINER (host PID $PID) -> $OUTDIR"

{
  echo "# T1 eBPF capture context"; echo
  echo "## host"; uname -a; grep PRETTY /etc/os-release
  echo; echo "## disk"; lsblk -d -o NAME,SIZE,ROTA,TYPE | grep -vE 'loop'
  echo; echo "## image"; docker inspect -f '{{.Config.Image}}' "$CONTAINER"
  echo "## threads (pid=$PID)"; for t in /proc/$PID/task/*; do cat "$t/comm"; done | sort | uniq -c | sort -rn
  echo; echo "## tools"; bpftrace --version; perf --version
} >"$OUTDIR/00-context.txt" 2>&1

echo ">> [1/5] fsync/fdatasync latency (60s)"
bpftrace "$HERE/fsync-latency.bt"     >"$OUTDIR/01-fsync-latency.txt" 2>&1 || true
echo ">> [2/5] block-I/O latency (60s)"
bpftrace "$HERE/bio-latency.bt"       >"$OUTDIR/02-bio-latency.txt" 2>&1 || true
echo ">> [3/5] off-CPU stacks (30s)"
bpftrace "$HERE/offcpu-qfcnode.bt"    >"$OUTDIR/03-offcpu-qfcnode.txt" 2>&1 || true
echo ">> [4/5] write-path syscalls (25s)"
bpftrace "$HERE/writepath-syscalls.bt" >"$OUTDIR/04-writepath-syscalls.txt" 2>&1 || true

echo ">> [5/5] on-CPU perf flame ($CONTAINER, 20s @ 99Hz)"
perf record -F 99 -g --call-graph fp -p "$PID" -o "$OUTDIR/perf.data" -- sleep 20 >/dev/null 2>&1 || true
perf report --stdio --no-children -i "$OUTDIR/perf.data" 2>/dev/null \
  | grep -vE '^#' | head -40 >"$OUTDIR/05-oncpu-report.txt" || true
# Collapse to folded stacks (merge +0x offsets; drop unresolved addresses).
perf script -i "$OUTDIR/perf.data" 2>/dev/null | awk '
  function emit(){ if(n>0){ s=fr[n]; for(i=n-1;i>=1;i--) s=s";"fr[i]; print s } n=0; delete fr }
  /^[ \t]/ { sym=$2; if(sym=="" || sym ~ /^0x/ || sym ~ /^[0-9a-f]+$/){next}
             sub(/\+0x[0-9a-f]+$/,"",sym); n++; fr[n]=sym; next }
  /^$/ { emit(); next }
  { emit() }
  END{ emit() }
' | sort | uniq -c | sort -rn \
  | awk '{c=$1;$1="";sub(/^ +/,"");gsub(/ /,"",$0);print $0" "c}' >"$OUTDIR/05-oncpu.folded" || true

echo ">> done. artifacts in $OUTDIR"
