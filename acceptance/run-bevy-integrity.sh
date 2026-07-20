#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 REPORT_ROOT BATCH_ID" >&2
  exit 2
fi

report_root=$1
batch_id=$2
repo=$(cd "$(dirname "$0")/.." && pwd)
report_root=$(mkdir -p "$report_root" && cd "$report_root" && pwd)
evidence_root=${CARGO_REAPI_EVIDENCE_ROOT:-$report_root}
evidence_root=$(cd "$evidence_root" && pwd)
driver=$repo/target/release/cargo-reapi
exec_auditor=$repo/target/release/cargo-reapi-exec-auditor
auditor_identity=${CARGO_REAPI_AUDITOR_IDENTITY:-$exec_auditor}
runner=$repo/acceptance/run-bevy-integrity.sh
case "$(uname -s)" in
  Darwin) platform=macos; profile=$repo/acceptance/platforms/macos-arm64.toml; format=macos-eslogger ;;
  Linux) platform=linux; profile=$repo/acceptance/platforms/linux-x86_64.toml; format=linux-strace ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac

. "$repo/acceptance/lib/provenance.sh"
write_intrinsic_run_start "$report_root" "$runner" "$driver" "$auditor_identity" "$profile" "$batch_id"
export CARGO_REAPI_BEVY_ROOT=$report_root/fixture
export CARGO_REAPI_CLONE_TRACE=$report_root/clone-selection-events.jsonl

cargo test --test bevy bevy_phase_prepare_deleted_producer -- --ignored --exact --nocapture \
  >"$report_root/prepare.log" 2>&1

real_rustc=$(rustup which rustc)
if [ "$platform" = macos ]; then
  linker=$(/usr/bin/xcrun --find clang)
  observer_kind=macos-eslogger
  observer_version=$(/usr/bin/eslogger --version 2>&1 || true)
  observer_command="sudo -n /usr/bin/eslogger --format json exec"
else
  linker=$(realpath "$(command -v cc)")
  observer_kind=linux-strace
  observer_version=$(strace --version | head -1)
  observer_command="strace -f -s 1048576 -v -e trace=execve"
fi
jq -n \
  --arg observer_kind "$observer_kind" --arg observer_version "$observer_version" \
  --arg observer_command "$observer_command" \
  --arg rustc "$real_rustc" --arg rustc_sha "$(sha256_file "$real_rustc")" \
  --arg linker "$linker" --arg linker_sha "$(sha256_file "$linker")" \
  '{schema_version:1,observer_kind:$observer_kind,observer_version:$observer_version,observer_command:[$observer_command],selected_executables:[{path:$rustc,sha256:$rustc_sha},{path:$linker,sha256:$linker_sha}],expected:"zero",attribution_root:null,expected_crates:[],coalescing_roots:[]}' \
  >"$report_root/warm-selection.json"

if [ "$platform" = macos ]; then
  events=$report_root/warm-os-events.jsonl
  sudo -n -l /usr/bin/eslogger >/dev/null
  perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' sudo -n /usr/bin/eslogger --format json exec \
    >"$events" 2>"$report_root/warm-observer.stderr" &
  observer_pid=$!
  sleep 1
  kill -0 "$observer_pid"
  status=0
  cargo test --test bevy bevy_phase_restore_under_os_observation -- --ignored --exact --nocapture \
    >"$report_root/warm.log" 2>&1 || status=$?
  kill -TERM "$observer_pid" 2>/dev/null || true
  wait "$observer_pid" 2>/dev/null || true
else
  events=$report_root/warm-os-events.strace
  status=0
  strace -f -s 1048576 -v -e trace=execve -o "$events" \
    cargo test --test bevy bevy_phase_restore_under_os_observation -- --ignored --exact --nocapture \
    >"$report_root/warm.log" 2>"$report_root/warm-observer.stderr" || status=$?
fi
test "$status" -eq 0

"$exec_auditor" --format "$format" --evidence-root "$evidence_root" \
  --events "$events" --observer-stderr "$report_root/warm-observer.stderr" \
  --selection-config "$report_root/warm-selection.json" \
  --normalized-events "$report_root/warm-normalized.jsonl" \
  --report "$report_root/warm-os-proof.json"

cargo test --test bevy bevy_phase_fresh_control_and_compare -- --ignored --exact --nocapture \
  >"$report_root/fresh.log" 2>&1

if [ "$platform" = macos ]; then
  claims=$(jq -cn '
    def claim($roles): {status:"PASS",evidence_roles:$roles};
    {application_parity:claim(["bevy_integrity_report"]),test_behavior_parity:claim(["bevy_integrity_report"]),consumer_paths_only:claim(["bevy_integrity_report"]),valid_signatures:claim(["bevy_integrity_report"]),zero_os_compiler_linker:claim(["warm_os_audit"])}')
else
  claims=$(jq -cn '
    def claim($roles): {status:"PASS",evidence_roles:$roles};
    {application_parity:claim(["bevy_integrity_report"]),test_behavior_parity:claim(["bevy_integrity_report"]),consumer_paths_only:claim(["bevy_integrity_report"]),elf_integrity:claim(["bevy_integrity_report","elf_evidence"]),zero_os_compiler_linker:claim(["warm_os_audit"])}')
fi
references="bevy_integrity_report:$report_root/fixture/bevy-integrity.json warm_os_audit:$report_root/warm-os-proof.json warm_test_log:$report_root/warm.log fresh_test_log:$report_root/fresh.log clone_selection_trace:$report_root/clone-selection-events.jsonl"
if [ "$platform" = linux ]; then
  for file in "$report_root"/fixture/*-readelf_*.txt "$report_root"/fixture/*-objdump_*.txt "$report_root"/fixture/*-ldd.txt "$report_root"/fixture/*-elflint.txt "$report_root"/fixture/*-strings.txt; do
    references="$references elf_evidence:$file"
  done
fi
# shellcheck disable=SC2086
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/bevy-integrity.receipt.json" bevy-integrity \
  "$report_root/run-start.json" "$claims" \
  "$(jq -c '{warm_elapsed_ms:.restored.warm_elapsed_ms,os_compiler_linker_events:0}' "$report_root/fixture/bevy-integrity.json")" \
  $references

echo "PASS  $report_root"
