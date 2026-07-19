#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 MORIA_ROOT SHARED_CACHE REPORT_ROOT" >&2
  exit 2
fi

moria_root=$1
shared_cache=$2
report_root=$3
driver=$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-reapi
contract=$(cd "$(dirname "$0")/.." && pwd)/acceptance/contract.toml

"$driver" contract verify --path "$contract"
"$driver" prove environment --report "$report_root/environment.json"
git -C "$moria_root" diff --quiet
git -C "$moria_root" diff --cached --quiet

mkdir -p "$shared_cache" "$report_root"
run_root=$(mktemp -d "$report_root/moria-proof.XXXXXX")

timestamp_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
}

copy_worktree() {
  destination=$1
  mkdir -p "$destination"
  rsync -a --exclude .git --exclude target "$moria_root/" "$destination/"
}

run_gate() {
  member_id=$1
  worktree=$2
  action_log=$3
  member_json=$4
  output_log=$5
  started=$(timestamp_ms)
  status=0
  (
    cd "$worktree"
    cargo fmt --all -- --check
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- check --all-targets
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- clippy --all-targets -- -D warnings
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- test
  ) >"$output_log" 2>&1 || status=$?
  completed=$(timestamp_ms)
  jq -n \
    --arg id "$member_id" \
    --arg action_log "$action_log" \
    --argjson started "$started" \
    --argjson completed "$completed" \
    --argjson exit_code "$status" \
    '{id:$id,started_at_unix_ms:$started,completed_at_unix_ms:$completed,exit_code:$exit_code,action_log:$action_log}' \
    >"$member_json"
  return "$status"
}

make_evidence() {
  destination=$1
  shift
  jq -s '{schema_version:1,members:.}' "$@" >"$destination"
}

run_population() {
  kind=$1
  count=$2
  population_root=$run_root/$kind
  evidence_root=$population_root/evidence
  mkdir -p "$population_root" "$evidence_root"
  index=1
  while [ "$index" -le "$count" ]; do
    copy_worktree "$population_root/member-$index"
    index=$((index + 1))
  done

  pids=""
  index=1
  while [ "$index" -le "$count" ]; do
    member_root=$population_root/member-$index
    run_gate \
      "$kind-$index" \
      "$member_root" \
      "$evidence_root/member-$index-actions.jsonl" \
      "$evidence_root/member-$index.json" \
      "$evidence_root/member-$index.log" &
    pids="$pids $!"
    index=$((index + 1))
  done
  population_status=0
  for pid in $pids; do
    wait "$pid" || population_status=1
  done
  evidence=$report_root/$kind-evidence.json
  make_evidence "$evidence" "$evidence_root"/member-*.json
  "$driver" prove population --kind "$kind" --evidence "$evidence" --report "$report_root/$kind-proof.json"
  return "$population_status"
}

producer=$run_root/producer
producer_evidence=$run_root/producer-evidence
copy_worktree "$producer"
mkdir -p "$producer_evidence"
run_gate \
  producer \
  "$producer" \
  "$producer_evidence/actions.jsonl" \
  "$producer_evidence/member.json" \
  "$producer_evidence/gate.log"
mv "$producer" "$run_root/producer-retired-and-unavailable"

run_population single 1
run_population five 5
run_population stress 10

echo "PASS  $report_root"
