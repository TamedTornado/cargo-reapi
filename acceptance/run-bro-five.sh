#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
  echo "usage: $0 REPORT_ROOT BATCH_ID BRO_ROOT MORIA_ROOT CACHE_DIR STORAGE_PROFILE" >&2
  exit 2
fi

report_root=$1
batch_id=$2
bro_root=$(cd "$3" && pwd)
moria_root=$(cd "$4" && pwd)
cache_dir=$(cd "$5" && pwd)
storage_profile=$6
repo=$(cd "$(dirname "$0")/.." && pwd)
report_root=$(mkdir -p "$report_root" && cd "$report_root" && pwd)
evidence_root=${CARGO_REAPI_EVIDENCE_ROOT:-$report_root}
evidence_root=$(cd "$evidence_root" && pwd)
driver=$repo/target/release/cargo-reapi
exec_auditor=$repo/target/release/cargo-reapi-exec-auditor
auditor_identity=${CARGO_REAPI_AUDITOR_IDENTITY:-$exec_auditor}
observer=$repo/acceptance/rustc-observer/rustc
runner=$repo/acceptance/run-bro-five.sh
harness=$bro_root/packages/pm/src/benchmarks/cargo-reapi-moria-proof.ts
case "$(uname -s)" in
  Darwin) platform=macos; format=macos-eslogger; profile=$repo/acceptance/platforms/macos-arm64.toml ;;
  Linux) platform=linux; format=linux-strace; profile=$repo/acceptance/platforms/linux-x86_64.toml ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac

. "$repo/acceptance/lib/provenance.sh"
write_intrinsic_run_start "$report_root" "$runner" "$driver" "$auditor_identity" "$profile" "$batch_id"
test -f "$harness"
git -C "$bro_root" diff --quiet -- packages/pm/src/benchmarks/cargo-reapi-moria-proof.ts
git -C "$bro_root" diff --cached --quiet -- packages/pm/src/benchmarks/cargo-reapi-moria-proof.ts
cp "$harness" "$report_root/bro-harness-source.ts"
git -C "$bro_root" rev-parse HEAD >"$report_root/bro-source-revision.txt"

real_rustc=$(rustup which rustc)
if [ "$platform" = macos ]; then linker=$(/usr/bin/xcrun --find clang); else linker=$(command -v cc); fi
selection_config() {
  destination=$1
  expected=$2
  jq -n \
    --arg observer_kind "$([ "$platform" = macos ] && echo macos-eslogger || echo linux-strace)" \
    --arg observer_version "$([ "$platform" = macos ] && (/usr/bin/eslogger --version 2>&1 || true) || strace --version | head -1)" \
    --arg rustc "$real_rustc" --arg rustc_sha "$(sha256_file "$real_rustc")" \
    --arg linker "$linker" --arg linker_sha "$(sha256_file "$linker")" \
    --arg expected "$expected" \
    '{schema_version:1,observer_kind:$observer_kind,observer_version:$observer_version,observer_command:["Bro public PM script under OS observer"],selected_executables:[{path:$rustc,sha256:$rustc_sha},{path:$linker,sha256:$linker_sha}],expected:$expected,attribution_root:null,expected_crates:[],coalescing_roots:[]}' \
    >"$destination"
}

run_bro() {
  pnpm --dir "$bro_root" --filter @bro/pm prove:moria:cargo-reapi -- \
    --repo "$moria_root" --cache-dir "$cache_dir" --driver "$driver" --observer "$observer" \
    --auditor "$exec_auditor" --report-dir "$report_root" --storage-profile "$storage_profile" "$@"
}

selection_config "$report_root/producer-selection.json" nonzero
status=0
if [ "$platform" = macos ]; then
  events=$report_root/producer-os-events.jsonl
  sudo -n -l /usr/bin/eslogger >/dev/null
  perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' sudo -n /usr/bin/eslogger --format json exec \
    >"$events" 2>"$report_root/producer-observer.stderr" &
  observer_pid=$!
  sleep 1
  kill -0 "$observer_pid"
  run_bro --producer-only true >"$report_root/bro-producer-cli.stdout" 2>"$report_root/bro-producer-cli.stderr" || status=$?
  kill -TERM "$observer_pid" 2>/dev/null || true
  wait "$observer_pid" 2>/dev/null || true
else
  events=$report_root/producer-os-events.strace
  strace -f -s 1048576 -v -e trace=execve -o "$events" \
    pnpm --dir "$bro_root" --filter @bro/pm prove:moria:cargo-reapi -- \
      --repo "$moria_root" --cache-dir "$cache_dir" --driver "$driver" --observer "$observer" \
      --auditor "$exec_auditor" --report-dir "$report_root" --storage-profile "$storage_profile" \
      --producer-only true \
      >"$report_root/bro-producer-cli.stdout" 2>"$report_root/bro-producer-cli.stderr" || status=$?
  : >"$report_root/producer-observer.stderr"
fi
test "$status" -eq 0

"$exec_auditor" --format "$format" --evidence-root "$evidence_root" \
  --events "$events" --observer-stderr "$report_root/producer-observer.stderr" \
  --selection-config "$report_root/producer-selection.json" \
  --normalized-events "$report_root/producer-normalized.jsonl" \
  --report "$report_root/producer-os-proof.json"
producer_retirement=$report_root/bro-producer-retirement.json
test -f "$producer_retirement"

selection_config "$report_root/consumers-selection.json" zero
status=0
if [ "$platform" = macos ]; then
  events=$report_root/consumers-os-events.jsonl
  perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' sudo -n /usr/bin/eslogger --format json exec \
    >"$events" 2>"$report_root/consumers-observer.stderr" &
  observer_pid=$!
  sleep 1
  kill -0 "$observer_pid"
  run_bro --consumers-only true --producer-retirement "$producer_retirement" \
    >"$report_root/bro-cli.stdout" 2>"$report_root/bro-cli.stderr" || status=$?
  kill -TERM "$observer_pid" 2>/dev/null || true
  wait "$observer_pid" 2>/dev/null || true
else
  events=$report_root/consumers-os-events.strace
  strace -f -s 1048576 -v -e trace=execve -o "$events" \
    pnpm --dir "$bro_root" --filter @bro/pm prove:moria:cargo-reapi -- \
      --repo "$moria_root" --cache-dir "$cache_dir" --driver "$driver" --observer "$observer" \
      --auditor "$exec_auditor" --report-dir "$report_root" --storage-profile "$storage_profile" \
      --consumers-only true --producer-retirement "$producer_retirement" \
      >"$report_root/bro-cli.stdout" 2>"$report_root/bro-cli.stderr" || status=$?
  : >"$report_root/consumers-observer.stderr"
fi
test "$status" -eq 0

"$exec_auditor" --format "$format" --evidence-root "$evidence_root" \
  --events "$events" --observer-stderr "$report_root/consumers-observer.stderr" \
  --selection-config "$report_root/consumers-selection.json" \
  --normalized-events "$report_root/consumers-normalized.jsonl" \
  --report "$report_root/consumers-os-proof.json"

claims=$(jq -cn '
  def claim($roles): {status:"PASS",evidence_roles:$roles};
  {public_cli_boundary:claim(["bro_cli_stdout","bro_harness_source"]),exact_environment_producer:claim(["bro_producer","producer_os_audit"]),producer_deleted:claim(["producer_retirement"]),five_jobs_simultaneous:claim(["bro_proof"]),canonical_gate:claim(["bro_proof","member_action_log"]),zero_physical_actions:claim(["bro_proof","member_action_log"]),zero_os_compiler_linker:claim(["bro_os_audit"]),deadline_met:claim(["bro_proof"])}')
references="bro_proof:$report_root/bro-moria-five-proof.json bro_population_proof:$report_root/bro-moria-five-population-proof.json bro_os_audit:$report_root/consumers-os-proof.json bro_producer:$report_root/bro-moria-producer.json producer_os_audit:$report_root/producer-os-proof.json producer_retirement:$producer_retirement bro_cli_stdout:$report_root/bro-cli.stdout bro_cli_stderr:$report_root/bro-cli.stderr bro_producer_cli_stdout:$report_root/bro-producer-cli.stdout bro_producer_cli_stderr:$report_root/bro-producer-cli.stderr bro_harness_source:$report_root/bro-harness-source.ts bro_source_revision:$report_root/bro-source-revision.txt"
for file in "$report_root"/member-*-actions.jsonl; do references="$references member_action_log:$file"; done
# shellcheck disable=SC2086
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/bro-five.receipt.json" bro-five \
  "$report_root/run-start.json" "$claims" \
  "$(jq -c '{members:.observed_members,elapsed_ms,physical_cacheable_actions:0,os_compiler_linker_events:0}' "$report_root/bro-moria-five-proof.json")" \
  $references

echo "PASS  $report_root"
