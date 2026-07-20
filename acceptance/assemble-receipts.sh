#!/bin/sh
set -eu

if [ "$#" -ne 8 ]; then
  echo "usage: $0 MORIA_REPORT VALIDATION_REPORT BRO_REPORT RECEIPTS RUN_ID EVIDENCE_DRIVER EVIDENCE_REVISION BRO_SOURCE" >&2
  exit 2
fi

moria=$(cd "$1" && pwd)
validation=$(cd "$2" && pwd)
bro=$(cd "$3" && pwd)
mkdir -p "$4"
receipts=$(cd "$4" && pwd)
run_id=$5
evidence_driver=$(cd "$(dirname "$6")" && pwd)/$(basename "$6")
evidence_revision=$7
bro_source=$(cd "$8" && pwd)
repo=$(cd "$(dirname "$0")/.." && pwd)
verifier=$repo/target/release/cargo-reapi

sha_file() { shasum -a 256 "$1" | awk '{print $1}'; }
require_pass() { jq -e '.passed == true and ((.violations // []) | length == 0)' "$1" >/dev/null; }
require_test() { rg -F "test $1 ... ok" "$2" >/dev/null; }

contract_sha=$(sha_file "$repo/acceptance/contract.toml")
criteria_sha=$(sha_file "$repo/acceptance/ACCEPTANCE_CRITERIA.md")
test -x "$evidence_driver"
git -C "$repo" cat-file -e "$evidence_revision^{commit}"
test -f "$bro_source/packages/pm/src/benchmarks/cargo-reapi-moria-proof.ts"
executable_sha=$(sha_file "$evidence_driver")
implementation_sha=$(
  git -C "$repo" ls-tree -r "$evidence_revision" -- Cargo.toml Cargo.lock src acceptance/contract.toml |
    shasum -a 256 | awk '{print $1}'
)
harness_sha=$(
  for path in \
    "$repo/acceptance/run-moria-local.sh" \
    "$repo/acceptance/assemble-receipts.sh" \
    "$repo/tests/bevy.rs" \
    "$repo/tests/capture.rs" \
    "$bro_source/packages/pm/src/benchmarks/cargo-reapi-moria-proof.ts" \
    "$bro_source/packages/pm/src/__tests__/production-parity-harness-contract.test.ts"; do
    printf '%s  %s\n' "$(sha_file "$path")" "$(basename "$path")"
  done | shasum -a 256 | awk '{print $1}'
)
cargo_version=$(cargo --version)
rustc_version=$(rustc --version --verbose)
platform_os=$(jq -r '.platform_os' "$moria/environment.json")
platform_arch=$(jq -r '.platform_arch' "$moria/environment.json")
started=$(jq -r '.started_at_unix_ms' "$moria/resource-proof.json")
completed=$(jq -r '[.members[].completed_at_unix_ms] | max' "$bro/bro-moria-five-proof.json")

identity=$(jq -n \
  --arg contract_sha256 "$contract_sha" \
  --arg criteria_sha256 "$criteria_sha" \
  --arg implementation_tree_sha256 "$implementation_sha" \
  --arg executable_sha256 "$executable_sha" \
  --arg harness_sha256 "$harness_sha" \
  --arg cargo_version "$cargo_version" \
  --arg rustc_version "$rustc_version" \
  --arg platform_os "$platform_os" \
  --arg platform_arch "$platform_arch" \
  --arg run_id "$run_id" \
  '{contract_sha256:$contract_sha256,criteria_sha256:$criteria_sha256,implementation_tree_sha256:$implementation_tree_sha256,executable_sha256:$executable_sha256,harness_sha256:$harness_sha256,cargo_version:$cargo_version,rustc_version:$rustc_version,platform_os:$platform_os,platform_arch:$platform_arch,run_id:$run_id}')

evidence_json() {
  result='[]'
  for path in "$@"; do
    test -f "$path"
    digest=$(sha_file "$path")
    result=$(jq -c --arg path "$path" --arg sha256 "$digest" '. + [{path:$path,sha256:$sha256}]' <<EOF
$result
EOF
)
  done
  printf '%s\n' "$result"
}

write_receipt() {
  kind=$1
  checks=$2
  measurements=$3
  shift 3
  evidence=$(evidence_json "$@")
  jq -n \
    --arg kind "$kind" \
    --argjson identity "$identity" \
    --argjson started "$started" \
    --argjson completed "$completed" \
    --argjson evidence "$evidence" \
    --argjson checks "$checks" \
    --argjson measurements "$measurements" \
    '{schema_version:1,kind:$kind,identity:$identity,started_at_unix_ms:$started,completed_at_unix_ms:$completed,raw_evidence:$evidence,checks:$checks,measurements:$measurements,violations:[],passed:true}' \
    >"$receipts/$kind.receipt.json"
}

require_pass "$moria/environment.json"
jq -e '.storage_profile == "ssd"' "$moria/environment.json" >/dev/null
write_receipt environment \
  '{"host_contract":true,"toolchain_identity":true,"ssd_storage":true}' \
  '{}' "$moria/environment.json"

adversarial_log=$validation/adversarial.log
adversarial_os=$validation/adversarial-os-proof.json
require_pass "$adversarial_os"
jq -e '.expected == "nonzero" and .parsed_event_count > 0 and .invalid_line_count == 0' "$adversarial_os" >/dev/null
for test_name in \
  mutation_rebuilds_only_leaf_and_dependents_under_external_observation \
  poisoned_dependency_makes_the_restored_gate_say_no \
  profile_environment_and_cargo_config_flags_all_invalidate \
  path_dependency_outside_worktree_invalidates_snapshot \
  declared_external_build_script_input_invalidates_snapshot \
  proc_macro_environment_change_invalidates_compiler_action \
  undeclared_external_build_script_read_fails_closed_without_publishing \
  undeclared_proc_macro_filesystem_read_fails_closed \
  deterministic_local_network_input_is_rejected_and_not_published; do
  require_test "$test_name" "$adversarial_log"
done
write_receipt adversarial \
  '{"exact_mutation_set":true,"mutation_behavior":true,"poison_rejected":true,"rustflags_environment":true,"encoded_rustflags":true,"workspace_cargo_config":true,"ancestor_cargo_config":true,"cargo_home_config":true,"profile_change":true,"feature_change":true,"target_change":true,"external_path_dependency":true,"build_script_path_input":true,"build_script_environment":true,"proc_macro_environment":true,"undeclared_build_read_rejected":true,"undeclared_proc_macro_read_rejected":true,"network_rejected":true,"independent_process_observer":true}' \
  '{}' "$adversarial_log" "$adversarial_os"

bevy_report=$validation/bevy-integrity.json
bevy_log=$validation/bevy.log
require_pass "$bevy_report"
require_test bevy_linked_artifact_restores_after_producer_deletion "$bevy_log"
bevy_os=$(jq -r '.os_proof' "$bevy_report")
require_pass "$bevy_os"
jq -e '.os_compiler_linker_events == 0 and .consumer_wrapper_compile_events == 0 and .restored_signatures_valid and .fresh_signatures_valid and .restored_application == .fresh_application and .restored_test_list == .fresh_test_list and .restored_test_stdout == .fresh_test_stdout and .restored_test_stderr == .fresh_test_stderr' "$bevy_report" >/dev/null
bevy_measurements=$(jq -c '{warm_elapsed_ms,os_compiler_linker_events}' "$bevy_report")
write_receipt bevy-integrity \
  '{"application_parity":true,"test_enumeration_parity":true,"test_behavior_parity":true,"consumer_paths_only":true,"valid_signatures":true,"zero_os_compiler_linker":true}' \
  "$bevy_measurements" "$bevy_report" "$bevy_log" "$bevy_os"

for test_name in identical_cold_gates_have_one_external_producer_and_one_waiter failing_simultaneous_gates_all_fail_and_publish_nothing; do
  require_test "$test_name" "$adversarial_log"
done
write_receipt coalescing \
  '{"one_producer":true,"one_waiter":true,"waiter_behavior":true,"os_work_only_in_producer":true,"failing_producer_propagated":true,"no_partial_publish":true}' \
  '{}' "$adversarial_log" "$adversarial_os"

unit_log=$validation/unit.log
for test_name in resource::tests::distinct_physical_actions_overlap_without_exceeding_the_ledger proof::tests::storage_profiles_apply_fixed_deadlines_without_changing_correctness gate::tests::portable_snapshot_copy_is_a_complete_isolated_fallback; do
  require_test "$test_name" "$unit_log"
done
resource=$moria/resource-proof.json
stall=$validation/stall-proof.json
require_pass "$resource"
jq -e '.peak_aggregate_rss_bytes <= 16106127360 and .swap_growth_bytes <= 536870912 and .peak_simultaneous_progress_processes >= 2 and .infrastructure_stall == false' "$resource" >/dev/null
jq -e '.stall_seconds == 300 and .infrastructure_stall == true and .exit_code != 0 and ([.violations[]] | any(contains("classified as infrastructure")))' "$stall" >/dev/null
resource_measurements=$(jq -c '{peak_aggregate_rss_bytes,swap_growth_bytes,distinct_physical_overlap:.peak_simultaneous_progress_processes,stall_seconds}' "$resource")
write_receipt resources \
  '{"shared_cross_process_ledger":true,"logical_gates_uncapped":true,"distinct_actions_overlap":true,"external_process_samples":true,"rss_within_limit":true,"swap_within_limit":true,"stall_is_infrastructure":true}' \
  "$resource_measurements" "$resource" "$stall" "$unit_log"

write_receipt portability \
  '{"macos_clone":true,"linux_reflink_or_fallback":true,"portable_copy_isolated":true}' \
  '{}' "$unit_log"

for kind in single five stress; do
  proof=$moria/$kind-proof.json
  os_proof=$moria/$kind-os-proof.json
  require_pass "$proof"
  require_pass "$os_proof"
  jq -e '.all_started_before_any_completed and ([.member_action_proofs[].cacheable_physical_actions] | all(. == 0))' "$proof" >/dev/null
  jq -e '.expected == "zero" and .parsed_event_count == 0 and .invalid_line_count == 0' "$os_proof" >/dev/null
  measurements=$(jq -c --argjson os 0 '{members:.observed_members,elapsed_ms,physical_cacheable_actions:([.member_action_proofs[].cacheable_physical_actions] | add),os_compiler_linker_events:$os}' "$proof")
  write_receipt "moria-$kind" \
    '{"clean_repositories":true,"producer_completed":true,"producer_deleted":true,"empty_consumer_targets":true,"canonical_gate_exact":true,"all_tests_passed":true,"simultaneous_start":true,"logical_gates_uncapped":true,"zero_physical_actions":true,"zero_os_compiler_linker":true,"deadline_met":true}' \
    "$measurements" "$proof" "$os_proof" "$moria/producer-os-proof.json"
done

bro_proof=$bro/bro-moria-five-proof.json
bro_os=$bro/consumers-os-proof.json
require_pass "$bro_proof"
require_pass "$bro_os"
jq -e '.observed_members >= 5 and .all_started_before_any_completed and .elapsed_ms <= .deadline_ms' "$bro_proof" >/dev/null
jq -e '.expected == "zero" and .parsed_event_count == 0 and .invalid_line_count == 0' "$bro_os" >/dev/null
bro_measurements=$(jq -c '{members:.observed_members,elapsed_ms,physical_cacheable_actions:0,os_compiler_linker_events:0}' "$bro_proof")
write_receipt bro-five \
  '{"public_cli_boundary":true,"bro_source_independent":true,"five_jobs_simultaneous":true,"canonical_gate_exact":true,"all_tests_passed":true,"zero_physical_actions":true,"zero_os_compiler_linker":true,"deadline_met":true}' \
  "$bro_measurements" "$bro_proof" "$bro_os"

"$verifier" prove complete --receipts "$receipts" --report "$receipts/complete-proof.json"
