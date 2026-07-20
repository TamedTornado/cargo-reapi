#!/bin/sh
set -eu

if [ "$#" -ne 9 ]; then
  echo "usage: $0 MEMBER WORKTREE DRIVER CACHE ACTION_LOG MEMBER_JSON OUTPUT_LOG OBSERVER REAL_RUSTC" >&2
  exit 2
fi

member_id=$1
worktree=$2
driver=$3
shared_cache=$4
action_log=$5
member_json=$6
output_log=$7
observer=$8
real_rustc=$9
rustc_trace=${CARGO_REAPI_RUSTC_TRACE:?CARGO_REAPI_RUSTC_TRACE is required}

timestamp_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
}

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
  cargo fmt --all -- --check || exit $?
  "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- check --all-targets || exit $?
  "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- clippy --all-targets -- -D warnings || exit $?
  "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- test || exit $?
) >"$output_log" 2>&1 || status=$?
completed=$(timestamp_ms)
jq -n \
  --arg id "$member_id" --arg action_log "$action_log" --arg worktree "$worktree" --arg rustc_trace "$rustc_trace" \
  --argjson started "$started" --argjson completed "$completed" --argjson exit_code "$status" \
  --argjson target_empty_at_start "$target_empty_at_start" \
  '{id:$id,started_at_unix_ms:$started,completed_at_unix_ms:$completed,exit_code:$exit_code,action_log:$action_log,worktree:$worktree,rustc_trace:$rustc_trace,target_empty_at_start:$target_empty_at_start}' \
  >"$member_json"
exit "$status"
