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
runner=$repo/acceptance/run-adversarial.sh
case "$(uname -s)" in
  Darwin) platform=macos; profile=$repo/acceptance/platforms/macos-arm64.toml ;;
  Linux) platform=linux; profile=$repo/acceptance/platforms/linux-x86_64.toml ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac

. "$repo/acceptance/lib/provenance.sh"
write_intrinsic_run_start "$report_root" "$runner" "$driver" "$auditor_identity" "$profile" "$batch_id"

real_rustc=$(rustup which rustc)
mutation_root=$report_root/exact-mutation
export CARGO_REAPI_MUTATION_ROOT=$mutation_root
cargo test --test capture exact_mutation_prepare_for_os_observation -- --ignored --exact --nocapture \
  >"$report_root/exact-mutation-prepare.log" 2>&1
consumer_root=$(cat "$mutation_root/consumer-root.txt")

selection_config() {
  destination=$1
  expected=$2
  attribution_root=$3
  observer_kind=$4
  observer_version=$5
  observer_command=$6
  rustc_sha=$(sha256_file "$real_rustc")
  if [ "$platform" = macos ]; then
    linker=$(/usr/bin/xcrun --find clang)
  else
    linker=$(realpath "$(command -v cc)")
  fi
  linker_sha=$(sha256_file "$linker")
  jq -n \
    --arg observer_kind "$observer_kind" \
    --arg observer_version "$observer_version" \
    --arg observer_command "$observer_command" \
    --arg rustc "$real_rustc" --arg rustc_sha "$rustc_sha" \
    --arg linker "$linker" --arg linker_sha "$linker_sha" \
    --arg expected "$expected" --arg attribution_root "$attribution_root" \
    '{schema_version:1,observer_kind:$observer_kind,observer_version:$observer_version,observer_command:[$observer_command],selected_executables:[{path:$rustc,sha256:$rustc_sha},{path:$linker,sha256:$linker_sha}],expected:$expected,attribution_root:(if $attribution_root=="" then null else $attribution_root end),expected_crates:(if $expected=="attribution" then ["leaf","mid","adversarial_app"] else [] end)}' \
    >"$destination"
}

pack_wrapper_trace() {
  trace=$1
  output=$2
  : >"$output"
  for record in "$trace"/*; do
    kind=$(sed -n 's/^kind=//p' "$record")
    crate_name=$(sed -n 's/^crate_name=//p' "$record")
    cwd=$(sed -n 's/^cwd=//p' "$record")
    jq -cn --arg kind "$kind" --arg crate_name "$crate_name" --arg cwd "$cwd" \
      '{kind:$kind,crate_name:$crate_name,cwd:$cwd}' >>"$output"
  done
}

run_observed() {
  label=$1
  expected=$2
  attribution_root=$3
  shift 3
  events=$report_root/$label-os-events.$([ "$platform" = macos ] && echo jsonl || echo strace)
  stderr=$report_root/$label-observer.stderr
  selection=$report_root/$label-selection.json
  normalized=$report_root/$label-normalized.jsonl
  report=$report_root/$label-os-proof.json
  if [ "$platform" = macos ]; then
    sudo -n -l /usr/bin/eslogger >/dev/null
    selection_config "$selection" "$expected" "$attribution_root" macos-eslogger "$(/usr/bin/eslogger --version 2>&1 || true)" "sudo -n /usr/bin/eslogger --format json exec"
    if [ "$expected" = coalescing ]; then
      temporary=$selection.$$.tmp
      jq --arg first "$coalescing_root/first" --arg second "$coalescing_root/second-with-different-length" '.coalescing_roots=[$first,$second]' "$selection" >"$temporary"
      mv "$temporary" "$selection"
    fi
    : >"$events"
    perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' sudo -n /usr/bin/eslogger --format json exec \
      >"$events" 2>"$stderr" &
    observer_pid=$!
    sleep 1
    kill -0 "$observer_pid"
    status=0
    "$@" || status=$?
    kill -TERM "$observer_pid" 2>/dev/null || true
    wait "$observer_pid" 2>/dev/null || true
    format=macos-eslogger
  else
    selection_config "$selection" "$expected" "$attribution_root" linux-strace "$(strace --version | head -1)" "strace -f -s 1048576 -v -e trace=execve"
    if [ "$expected" = coalescing ]; then
      temporary=$selection.$$.tmp
      jq --arg first "$coalescing_root/first" --arg second "$coalescing_root/second-with-different-length" '.coalescing_roots=[$first,$second]' "$selection" >"$temporary"
      mv "$temporary" "$selection"
    fi
    status=0
    strace -f -s 1048576 -v -e trace=execve -o "$events" "$@" 2>"$stderr" || status=$?
    format=linux-strace
  fi
  extra=
  if [ "$expected" = attribution ]; then
    pack_wrapper_trace "$mutation_root/consumer-wrapper-trace" "$report_root/$label-wrapper-attribution.jsonl"
    extra=$report_root/$label-wrapper-attribution.jsonl
  fi
  if [ -n "$extra" ]; then
    "$exec_auditor" --format "$format" --evidence-root "$evidence_root" \
      --events "$events" --observer-stderr "$stderr" --selection-config "$selection" \
      --normalized-events "$normalized" --wrapper-trace "$extra" --report "$report"
  else
    "$exec_auditor" --format "$format" --evidence-root "$evidence_root" \
      --events "$events" --observer-stderr "$stderr" --selection-config "$selection" \
      --normalized-events "$normalized" --report "$report"
  fi
  return "$status"
}

run_observed exact-mutation attribution "$consumer_root" \
  cargo test --test capture exact_mutation_consumer_under_os_observation -- --ignored --exact --nocapture \
  >"$report_root/exact-mutation-consumer.log" 2>&1

coalescing_root=$report_root/exact-coalescing
export CARGO_REAPI_COALESCING_ROOT=$coalescing_root
run_observed exact-coalescing coalescing "" \
  cargo test --test capture exact_coalescing_under_os_observation -- --ignored --exact --nocapture \
  >"$report_root/exact-coalescing.log" 2>&1

run_observed adversarial-suite nonzero "" \
  cargo test --test capture -- --test-threads=1 \
  >"$report_root/adversarial-suite.log" 2>&1

cargo test --bin cargo-reapi gate::tests::portable_snapshot_copy_is_a_complete_isolated_fallback \
  >"$report_root/portable-copy.log" 2>&1

claims=$(jq -cn '
  def claim($roles): {status:"PASS",evidence_roles:$roles};
  {
    exact_mutation_set_os:claim(["exact_mutation_os_audit"]),
    wrapper_attribution_crosscheck:claim(["wrapper_attribution_crosscheck"]),
    mutation_behavior:claim(["exact_mutation_consumer_log"]),
    poison_rejected:claim(["adversarial_suite_log"]),
    flags_and_cargo_configuration:claim(["adversarial_suite_log"]),
    external_and_generated_inputs:claim(["adversarial_suite_log"]),
    undeclared_reads_rejected:claim(["adversarial_suite_log"]),
    network_rejected:claim(["adversarial_suite_log"])
  }')
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/adversarial.receipt.json" adversarial \
  "$report_root/run-start.json" "$claims" '{}' \
  "exact_mutation_os_audit:$report_root/exact-mutation-os-proof.json" \
  "exact_mutation_consumer_log:$report_root/exact-mutation-consumer.log" \
  "adversarial_suite_os_audit:$report_root/adversarial-suite-os-proof.json" \
  "adversarial_suite_log:$report_root/adversarial-suite.log"

coalescing_claims=$(jq -cn '
  def claim($roles): {status:"PASS",evidence_roles:$roles};
  {one_producer_one_waiter:claim(["coalescing_os_audit","coalescing_result"]),waiter_behavior:claim(["coalescing_result"]),os_work_only_in_producer:claim(["coalescing_os_audit"]),failure_propagated:claim(["adversarial_suite_log"]),no_partial_publish:claim(["adversarial_suite_log"])}')
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/coalescing.receipt.json" coalescing \
  "$report_root/run-start.json" "$coalescing_claims" '{}' \
  "coalescing_os_audit:$report_root/exact-coalescing-os-proof.json" \
  "coalescing_result:$coalescing_root/coalescing-result.json" \
  "coalescing_test_log:$report_root/exact-coalescing.log" \
  "adversarial_suite_log:$report_root/adversarial-suite.log"

portable_claims=$(jq -cn '{portable_copy_isolated:{status:"PASS",evidence_roles:["portable_copy_test_log"]}}')
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/portable-copy-isolated.receipt.json" portable-copy-isolated \
  "$report_root/run-start.json" "$portable_claims" '{}' \
  "portable_copy_test_log:$report_root/portable-copy.log"

echo "PASS  $report_root"
