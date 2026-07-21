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
work_root=$(mktemp -d "$report_root/resource-cold.XXXXXX")
lane_a=$work_root/lane-a
lane_b=$work_root/lane-b
bevy_link=$work_root/bevy-link
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
mkdir -p "$lane_a" "$lane_b" "$bevy_link/src" "$bevy_link/tests"
mkdir -p "$report_root/lane-a-rustc-trace" "$report_root/lane-b-rustc-trace"
mkdir -p "$report_root/bevy-link-rustc-trace"
git -C "$moria_root" archive --format=tar HEAD | tar -xf - -C "$lane_a"
git -C "$moria_root" archive --format=tar HEAD | tar -xf - -C "$lane_b"
cp "$repo/acceptance/bevy-fixture/Cargo.toml" "$repo/acceptance/bevy-fixture/Cargo.lock" "$bevy_link"
cp "$repo/acceptance/bevy-fixture/src/main.rs" "$bevy_link/src/main.rs"
cp "$repo/acceptance/bevy-fixture/tests/runtime.rs" "$bevy_link/tests/runtime.rs"

timestamp_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
}

run_check_lane() {
  id=$1
  worktree=$2
  debug=$3
  action_log=$report_root/$id-actions.jsonl
  member=$report_root/$id-member.json
  output=$report_root/$id-output.log
  trace=$report_root/$id-rustc-trace
  target_empty_at_start=true
  if [ -d "$worktree/target" ] && find "$worktree/target" -mindepth 1 -print -quit | grep -q .; then
    target_empty_at_start=false
  fi
  started=$(timestamp_ms)
  status=0
  (
    cd "$worktree"
    export RUSTC="$observer"
    export CARGO_REAPI_REAL_RUSTC="$real_rustc"
    export CARGO_REAPI_RUSTC_TRACE="$trace"
    export CARGO_PROFILE_DEV_DEBUG="$debug"
    cargo fmt --all -- --check
    "$driver" --backend cache --cache-dir "$cache_dir" --action-log "$action_log" -- check --all-targets
  ) >"$output" 2>&1 || status=$?
  completed=$(timestamp_ms)
  jq -n --arg id "$id" --arg action_log "$action_log" --arg worktree "$worktree" \
    --arg rustc_trace "$trace" --argjson started "$started" --argjson completed "$completed" \
    --argjson exit_code "$status" --argjson target_empty_at_start "$target_empty_at_start" \
    '{id:$id,started_at_unix_ms:$started,completed_at_unix_ms:$completed,exit_code:$exit_code,action_log:$action_log,worktree:$worktree,rustc_trace:$rustc_trace,target_empty_at_start:$target_empty_at_start}' \
    >"$member"
  return "$status"
}

run_check_lane lane-a "$lane_a" 1 &
pid_a=$!
run_check_lane lane-b "$lane_b" 2 &
pid_b=$!

status_a=0
status_b=0
wait "$pid_a" || status_a=$?
pid_a=
wait "$pid_b" || status_b=$?
pid_b=
test "$status_a" -eq 0
test "$status_b" -eq 0

bevy_link_target_empty_at_start=true
if [ -d "$bevy_link/target" ] && find "$bevy_link/target" -mindepth 1 -print -quit | grep -q .; then
  bevy_link_target_empty_at_start=false
fi
bevy_link_started=$(timestamp_ms)
bevy_link_status=0
(
  cd "$bevy_link"
  export RUSTC="$observer"
  export CARGO_REAPI_REAL_RUSTC="$real_rustc"
  export CARGO_REAPI_RUSTC_TRACE="$report_root/bevy-link-rustc-trace"
  "$driver" --backend cache --cache-dir "$cache_dir" \
    --action-log "$report_root/bevy-link-actions.jsonl" -- test --no-run
) >"$report_root/bevy-link-output.log" 2>&1 || bevy_link_status=$?
bevy_link_completed=$(timestamp_ms)
jq -n --arg id bevy-link --arg action_log "$report_root/bevy-link-actions.jsonl" \
  --arg worktree "$bevy_link" --arg rustc_trace "$report_root/bevy-link-rustc-trace" \
  --argjson started "$bevy_link_started" --argjson completed "$bevy_link_completed" \
  --argjson exit_code "$bevy_link_status" --argjson target_empty_at_start "$bevy_link_target_empty_at_start" \
  '{id:$id,started_at_unix_ms:$started,completed_at_unix_ms:$completed,exit_code:$exit_code,action_log:$action_log,worktree:$worktree,rustc_trace:$rustc_trace,target_empty_at_start:$target_empty_at_start}' \
  >"$report_root/bevy-link-member.json"
test "$bevy_link_status" -eq 0

jq -s -e '
  length == 2 and
  all(.[]; .exit_code == 0 and .target_empty_at_start == true) and
  ([.[].started_at_unix_ms] | max) < ([.[].completed_at_unix_ms] | min)
' "$report_root/lane-a-member.json" "$report_root/lane-b-member.json" >/dev/null
jq -s -e '
  ([.[] | select(.execution == "local-cache-miss" and .exit_code == 0)] | length) > 0 and
  ([.[] | select(.cache_eligibility.eligible == true and .exit_code != 0)] | length) == 0
' "$report_root/lane-a-actions.jsonl" >/dev/null
jq -s -e '
  ([.[] | select(.execution == "local-cache-miss" and .exit_code == 0)] | length) > 0 and
  ([.[] | select(.cache_eligibility.eligible == true and .exit_code != 0)] | length) == 0
' "$report_root/lane-b-actions.jsonl" >/dev/null
jq -s -e '
  ([.[] | select(.execution == "local-cache-miss" and .exit_code == 0)] | length) > 0 and
  ([.[] | select(.cache_eligibility.eligible == true and .exit_code != 0)] | length) == 0
' "$report_root/bevy-link-actions.jsonl" >/dev/null

echo "PASS  $report_root"
