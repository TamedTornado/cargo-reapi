#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
  echo "usage: $0 MORIA_ROOT CACHE_DIR REPORT_ROOT DRIVER OBSERVER REAL_RUSTC" >&2
  exit 2
fi

moria_root=$(cd "$1" && pwd)
cache_dir=$(mkdir -p "$2" && cd "$2" && pwd)
report_root=$(mkdir -p "$3" && cd "$3" && pwd)
driver=$4
observer=$5
real_rustc=$6
repo=$(cd "$(dirname "$0")/.." && pwd)
gate_runner=$repo/acceptance/run-moria-gate.sh
work_root=$(mktemp -d "$report_root/resource-cold.XXXXXX")
lane_a=$work_root/lane-a
lane_b=$work_root/lane-b
pid_a=
pid_b=

cleanup() {
  if [ -n "$pid_a" ]; then kill -TERM "$pid_a" 2>/dev/null || true; fi
  if [ -n "$pid_b" ]; then kill -TERM "$pid_b" 2>/dev/null || true; fi
  rm -rf "$work_root"
}
trap cleanup EXIT HUP INT TERM

git -C "$moria_root" diff --quiet
git -C "$moria_root" diff --cached --quiet
mkdir -p "$lane_a" "$lane_b"
mkdir -p "$report_root/lane-a-rustc-trace" "$report_root/lane-b-rustc-trace"
git -C "$moria_root" archive --format=tar HEAD | tar -xf - -C "$lane_a"
git -C "$moria_root" archive --format=tar HEAD | tar -xf - -C "$lane_b"

CARGO_REAPI_RUSTC_TRACE=$report_root/lane-a-rustc-trace \
CARGO_PROFILE_DEV_DEBUG=1 \
  "$gate_runner" lane-a "$lane_a" "$driver" "$cache_dir" \
    "$report_root/lane-a-actions.jsonl" "$report_root/lane-a-member.json" \
    "$report_root/lane-a-output.log" "$observer" "$real_rustc" &
pid_a=$!
CARGO_REAPI_RUSTC_TRACE=$report_root/lane-b-rustc-trace \
CARGO_PROFILE_DEV_DEBUG=2 \
  "$gate_runner" lane-b "$lane_b" "$driver" "$cache_dir" \
    "$report_root/lane-b-actions.jsonl" "$report_root/lane-b-member.json" \
    "$report_root/lane-b-output.log" "$observer" "$real_rustc" &
pid_b=$!

status_a=0
status_b=0
wait "$pid_a" || status_a=$?
pid_a=
wait "$pid_b" || status_b=$?
pid_b=
test "$status_a" -eq 0
test "$status_b" -eq 0

jq -s -e '
  length == 2 and
  all(.[]; .exit_code == 0 and .target_empty_at_start == true) and
  ([.[].started_at_unix_ms] | max) < ([.[].completed_at_unix_ms] | min)
' "$report_root/lane-a-member.json" "$report_root/lane-b-member.json" >/dev/null
jq -s -e '
  all(.[]; .exit_code == 0) and
  ([.[] | select(.execution == "local-cache-miss")] | length) > 0
' "$report_root/lane-a-actions.jsonl" >/dev/null
jq -s -e '
  all(.[]; .exit_code == 0) and
  ([.[] | select(.execution == "local-cache-miss")] | length) > 0
' "$report_root/lane-b-actions.jsonl" >/dev/null

echo "PASS  $report_root"
