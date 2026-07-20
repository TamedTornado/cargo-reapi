#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: $0 REPORT_ROOT BATCH_ID RESOURCE_REPORT LEDGER_ROOT STALL_LEDGER_ROOT" >&2
  exit 2
fi

report_root=$1
batch_id=$2
resource_report=$3
ledger_root=$4
stall_ledger_root=$5
repo=$(cd "$(dirname "$0")/.." && pwd)
report_root=$(mkdir -p "$report_root" && cd "$report_root" && pwd)
evidence_root=${CARGO_REAPI_EVIDENCE_ROOT:-$report_root}
evidence_root=$(cd "$evidence_root" && pwd)
driver=$repo/target/release/cargo-reapi
resource_auditor=$repo/target/release/cargo-reapi-auditor
auditor_identity=${CARGO_REAPI_AUDITOR_IDENTITY:-$resource_auditor}
runner=$repo/acceptance/run-resources.sh
case "$(uname -s)" in
  Darwin) profile=$repo/acceptance/platforms/macos-arm64.toml ;;
  Linux) profile=$repo/acceptance/platforms/linux-x86_64.toml ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac

. "$repo/acceptance/lib/provenance.sh"
write_intrinsic_run_start "$report_root" "$runner" "$driver" "$auditor_identity" "$profile" "$batch_id"

test -d "$ledger_root"
test -f "$resource_report"
jq -e '.passed == true and (.violations | length) == 0 and .peak_aggregate_rss_bytes <= 16106127360 and .swap_growth_bytes <= 536870912 and .peak_simultaneous_progress_processes >= 2 and .observed_lease_owners > 0 and (.observed_action_identities | length) >= 2 and .infrastructure_stall == false' "$resource_report" >/dev/null

mkdir -p "$stall_ledger_root"
stall_status=0
"$resource_auditor" run --report "$report_root/stall-report.json" --ledger-root "$stall_ledger_root" \
  --stall-seconds 300 -- /bin/sleep 400 >"$report_root/stall-auditor.stdout" 2>"$report_root/stall-auditor.stderr" || stall_status=$?
test "$stall_status" -ne 0
jq -e '.stall_seconds == 300 and .infrastructure_stall == true and .exit_code != 0 and ([.violations[]] | any(contains("classified as infrastructure")))' "$report_root/stall-report.json" >/dev/null

cargo test --bin cargo-reapi resource::tests::distinct_physical_actions_overlap_without_exceeding_the_ledger \
  >"$report_root/ledger-unit.log" 2>&1

claims=$(jq -cn '
  def claim($roles): {status:"PASS",evidence_roles:$roles};
  {shared_ledger:claim(["resource_report","ledger_unit_log"]),distinct_actions_overlap:claim(["resource_report"]),rss_within_limit:claim(["resource_report"]),swap_within_limit:claim(["resource_report"]),stall_is_infrastructure:claim(["stall_report","stall_auditor_stderr"])}')
measurements=$(jq -c '{peak_aggregate_rss_bytes,swap_growth_bytes,distinct_physical_overlap:.peak_simultaneous_progress_processes,observed_lease_owners,observed_action_identities:(.observed_action_identities|length),stall_seconds:300}' "$resource_report")
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/resources.receipt.json" resources \
  "$report_root/run-start.json" "$claims" "$measurements" \
  "resource_report:$resource_report" \
  "stall_report:$report_root/stall-report.json" \
  "stall_auditor_stdout:$report_root/stall-auditor.stdout" \
  "stall_auditor_stderr:$report_root/stall-auditor.stderr" \
  "ledger_unit_log:$report_root/ledger-unit.log"

echo "PASS  $report_root"
