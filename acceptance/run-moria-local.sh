#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: $0 MORIA_ROOT SHARED_CACHE REPORT_ROOT STORAGE_PROFILE" >&2
  exit 2
fi

moria_root=$1
shared_cache=$2
report_root=$3
storage_profile=$4
case "$storage_profile" in
  ssd|rotational) ;;
  *) echo "storage profile must be ssd or rotational" >&2; exit 2 ;;
esac
driver=$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-reapi
auditor=$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-reapi-auditor
contract=$(cd "$(dirname "$0")/.." && pwd)/acceptance/contract.toml
observer=$(cd "$(dirname "$0")/.." && pwd)/acceptance/rustc-observer/rustc
real_rustc=$(command -v rustc)
observed_rustc=$(rustup which rustc)
observed_clang=$(/usr/bin/xcrun --find clang)

if ! sudo -n true 2>/dev/null; then
  echo "acceptance requires an operator-authorized sudo session for eslogger (run sudo -v first)" >&2
  exit 2
fi

start_os_observer() {
  os_events=$1
  : >"$os_events"
  sudo -n /usr/bin/eslogger --format json \
    --select "$observed_rustc" --select "$observed_clang" exec \
    >"$os_events" 2>"$os_events.stderr" &
  os_observer_pid=$!
  sleep 1
  kill -0 "$os_observer_pid"
}

stop_os_observer() {
  expected=$1
  proof=$2
  kill -INT "$os_observer_pid" 2>/dev/null || true
  wait "$os_observer_pid" 2>/dev/null || true
  "$auditor" eslog --events "$os_events" --expected "$expected" --report "$proof"
}

"$driver" contract verify --path "$contract"
"$driver" prove environment --storage-profile "$storage_profile" --report "$report_root/environment.json"
git -C "$moria_root" diff --quiet
git -C "$moria_root" diff --cached --quiet

mkdir -p "$shared_cache" "$report_root"
run_root=$(mktemp -d "$report_root/moria-proof.XXXXXX")
rustc_trace=$run_root/rustc-trace
mkdir -p "$rustc_trace"

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
    export RUSTC="$observer"
    export CARGO_REAPI_REAL_RUSTC="$real_rustc"
    export CARGO_REAPI_RUSTC_TRACE="$rustc_trace"
    cargo fmt --all -- --check || exit $?
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- check --all-targets || exit $?
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- clippy --all-targets -- -D warnings || exit $?
    "$driver" --backend cache --cache-dir "$shared_cache" --action-log "$action_log" -- test || exit $?
  ) >"$output_log" 2>&1 || status=$?
  completed=$(timestamp_ms)
  jq -n \
    --arg id "$member_id" \
    --arg action_log "$action_log" \
    --arg worktree "$worktree" \
    --arg rustc_trace "$rustc_trace" \
    --argjson started "$started" \
    --argjson completed "$completed" \
    --argjson exit_code "$status" \
    '{id:$id,started_at_unix_ms:$started,completed_at_unix_ms:$completed,exit_code:$exit_code,action_log:$action_log,worktree:$worktree,rustc_trace:$rustc_trace}' \
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

  start_os_observer "$evidence_root/os-compiler-linker-events.jsonl"
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
  stop_os_observer zero "$report_root/$kind-os-proof.json"
  evidence=$report_root/$kind-evidence.json
  make_evidence "$evidence" "$evidence_root"/member-*.json
  "$driver" prove population --kind "$kind" --storage-profile "$storage_profile" --evidence "$evidence" --report "$report_root/$kind-proof.json"
  index=1
  while [ "$index" -le "$count" ]; do
    rm -rf "$population_root/member-$index"
    index=$((index + 1))
  done
  return "$population_status"
}

producer=$run_root/producer
producer_evidence=$run_root/producer-evidence
copy_worktree "$producer"
mkdir -p "$producer_evidence"
start_os_observer "$producer_evidence/os-compiler-linker-events.jsonl"
run_gate \
  producer \
  "$producer" \
  "$producer_evidence/actions.jsonl" \
  "$producer_evidence/member.json" \
  "$producer_evidence/gate.log"
stop_os_observer nonzero "$report_root/producer-os-proof.json"
mv "$producer" "$run_root/producer-retired-and-unavailable"
rm -rf "$run_root/producer-retired-and-unavailable"
test ! -e "$run_root/producer-retired-and-unavailable"

run_population single 1
run_population five 5
run_population stress 10

echo "PASS  $report_root"
