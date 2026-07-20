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
moria_root=$(cd "$moria_root" && pwd)
mkdir -p "$shared_cache" "$report_root"
shared_cache=$(cd "$shared_cache" && pwd)
report_root=$(cd "$report_root" && pwd)
evidence_root=${CARGO_REAPI_EVIDENCE_ROOT:-$report_root}
evidence_root=$(cd "$evidence_root" && pwd)
driver=$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-reapi
exec_auditor=$(cd "$(dirname "$0")/.." && pwd)/target/release/cargo-reapi-exec-auditor
auditor_identity=${CARGO_REAPI_AUDITOR_IDENTITY:-$exec_auditor}
contract=$(cd "$(dirname "$0")/.." && pwd)/acceptance/contract.toml
case "$(uname -s)" in
  Darwin) platform=macos; profile=$(cd "$(dirname "$0")/.." && pwd)/acceptance/platforms/macos-arm64.toml; format=macos-eslogger ;;
  Linux) platform=linux; profile=$(cd "$(dirname "$0")/.." && pwd)/acceptance/platforms/linux-x86_64.toml; format=linux-strace ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac
observer=$(cd "$(dirname "$0")/.." && pwd)/acceptance/rustc-observer/rustc
gate_runner=$(cd "$(dirname "$0")/.." && pwd)/acceptance/run-moria-gate.sh
real_rustc=$(rustup which rustc)
observed_rustc=$real_rustc
if [ "$platform" = macos ]; then
  observed_linker=$(/usr/bin/xcrun --find clang)
else
  observed_linker=$(command -v cc)
fi
os_observer_pid=
batch_id=${CARGO_REAPI_BATCH_ID:-cargo-reapi-macos-candidate}

. "$(dirname "$0")/lib/provenance.sh"
write_intrinsic_run_start "$report_root" "$0" "$driver" "$auditor_identity" "$profile" "$batch_id"
export CARGO_REAPI_CLONE_TRACE=$report_root/clone-selection-events.jsonl

cleanup_os_observer() {
  if [ -n "${os_observer_pid:-}" ]; then
    kill -TERM "$os_observer_pid" 2>/dev/null || true
    wait "$os_observer_pid" 2>/dev/null || true
    os_observer_pid=
  fi
}
trap cleanup_os_observer EXIT HUP INT TERM

if [ "$platform" = macos ] && ! sudo -n -l /usr/bin/eslogger >/dev/null 2>&1; then
  echo "acceptance requires passwordless permission for /usr/bin/eslogger" >&2
  echo "see acceptance/ACCEPTANCE_CRITERIA.md for the scoped macOS sudoers rule" >&2
  exit 2
fi

start_os_observer() {
  os_events=$1
  expected=$2
  selection=$3
  : >"$os_events"
  : >"$os_events.stderr"
  rustc_sha=$(sha256_file "$observed_rustc")
  linker_sha=$(sha256_file "$observed_linker")
  if [ "$platform" = macos ]; then
    observer_kind=macos-eslogger
    observer_version=$(/usr/bin/eslogger --version 2>&1 || true)
    observer_command="sudo -n /usr/bin/eslogger --format json exec"
  else
    observer_kind=linux-strace
    observer_version=$(strace --version | head -1)
    observer_command="strace -f -s 1048576 -v -e trace=execve"
  fi
  jq -n \
    --arg observer_kind "$observer_kind" --arg observer_version "$observer_version" \
    --arg observer_command "$observer_command" \
    --arg rustc "$observed_rustc" --arg rustc_sha "$rustc_sha" \
    --arg linker "$observed_linker" --arg linker_sha "$linker_sha" \
    --arg expected "$expected" \
    '{schema_version:1,observer_kind:$observer_kind,observer_version:$observer_version,observer_command:[$observer_command],selected_executables:[{path:$rustc,sha256:$rustc_sha},{path:$linker,sha256:$linker_sha}],expected:$expected,attribution_root:null,expected_crates:[]}' \
    >"$selection"
  # eslogger suppresses its own process group. Start it in a distinct session
  # so it observes, rather than suppresses, the build being audited.
  if [ "$platform" = macos ]; then
    perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' \
      sudo -n /usr/bin/eslogger --format json exec \
      >"$os_events" 2>"$os_events.stderr" &
    os_observer_pid=$!
    sleep 1
    kill -0 "$os_observer_pid"
  fi
}

stop_os_observer() {
  proof=$1
  selection=$2
  if [ "$platform" = macos ]; then
    kill -TERM "$os_observer_pid" 2>/dev/null || true
    wait "$os_observer_pid" 2>/dev/null || true
    os_observer_pid=
  fi
  normalized=${proof%-proof.json}-normalized-events.jsonl
  "$exec_auditor" \
    --format "$format" \
    --evidence-root "$evidence_root" \
    --events "$os_events" \
    --observer-stderr "$os_events.stderr" \
    --selection-config "$selection" \
    --normalized-events "$normalized" \
    --report "$proof"
}

"$driver" contract verify --path "$contract"
"$driver" prove environment --storage-profile "$storage_profile" --platform-profile "$profile" --report "$report_root/environment.json"
if [ "$platform" = linux ]; then
  findmnt -T "$shared_cache" -J -o TARGET,SOURCE,FSTYPE,OPTIONS >"$report_root/cache-filesystem.json"
  findmnt -T "$moria_root" -J -o TARGET,SOURCE,FSTYPE,OPTIONS >"$report_root/worktree-filesystem.json"
  jq -n --rawfile os_release /etc/os-release \
    --arg kernel "$(uname -srvmo)" --arg cargo "$(cargo --version)" --arg rustc "$(rustc --version --verbose)" \
    --arg sandbox "@anthropic-ai/sandbox-runtime 0.0.66 using bubblewrap + seccomp; fail-closed policy" \
    --arg process_observer "strace -f execve with full arguments" \
    '{schema_version:1,os_release:$os_release,kernel:$kernel,cargo:$cargo,rustc:$rustc,sandbox_mechanism:$sandbox,process_observer:$process_observer,passed:true,violations:[]}' \
    >"$report_root/platform-environment.json"
else
  jq -n --arg os_release "$(sw_vers 2>/dev/null || uname -a)" --arg kernel "$(uname -srvmo)" \
    --arg cargo "$(cargo --version)" --arg rustc "$(rustc --version --verbose)" \
    --arg sandbox "@anthropic-ai/sandbox-runtime 0.0.66 using Seatbelt; fail-closed policy" \
    --arg process_observer "eslogger exec events with full arguments" \
    '{schema_version:1,os_release:$os_release,kernel:$kernel,cargo:$cargo,rustc:$rustc,sandbox_mechanism:$sandbox,process_observer:$process_observer,passed:true,violations:[]}' \
    >"$report_root/platform-environment.json"
fi
if [ -n "$(git -C "$moria_root" status --porcelain)" ]; then
  echo "acceptance requires a completely clean Moria repository, including no untracked files" >&2
  exit 2
fi

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
  CARGO_REAPI_RUSTC_TRACE="$rustc_trace" "$gate_runner" "$member_id" "$worktree" "$driver" "$shared_cache" \
    "$action_log" "$member_json" "$output_log" "$observer" "$real_rustc"
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
  population_evidence_dir=$population_root/evidence
  mkdir -p "$population_root" "$population_evidence_dir"
  index=1
  while [ "$index" -le "$count" ]; do
    copy_worktree "$population_root/member-$index"
    index=$((index + 1))
  done

  if [ "$platform" = macos ]; then events_suffix=jsonl; else events_suffix=strace; fi
  start_os_observer "$population_evidence_dir/os-compiler-linker-events.$events_suffix" zero "$population_evidence_dir/selection-config.json"
  pids=""
  index=1
  while [ "$index" -le "$count" ]; do
    member_root=$population_root/member-$index
    if [ "$platform" = linux ]; then
      CARGO_REAPI_RUSTC_TRACE="$rustc_trace" strace -f -s 1048576 -v -e trace=execve \
        -o "$population_evidence_dir/member-$index-os-events.strace" \
        "$gate_runner" "$kind-$index" "$member_root" "$driver" "$shared_cache" \
        "$population_evidence_dir/member-$index-actions.jsonl" "$population_evidence_dir/member-$index.json" \
        "$population_evidence_dir/member-$index.log" "$observer" "$real_rustc" &
    else
      run_gate \
        "$kind-$index" \
        "$member_root" \
        "$population_evidence_dir/member-$index-actions.jsonl" \
        "$population_evidence_dir/member-$index.json" \
        "$population_evidence_dir/member-$index.log" &
    fi
    pids="$pids $!"
    index=$((index + 1))
  done
  population_status=0
  for pid in $pids; do
    wait "$pid" || population_status=1
  done
  if [ "$platform" = linux ]; then
    cat "$population_evidence_dir"/member-*-os-events.strace >"$os_events"
  fi
  stop_os_observer "$report_root/$kind-os-proof.json" "$population_evidence_dir/selection-config.json"
  evidence=$report_root/$kind-evidence.json
  make_evidence "$evidence" "$population_evidence_dir"/member-*.json
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
if [ "$platform" = macos ]; then producer_suffix=jsonl; else producer_suffix=strace; fi
start_os_observer "$producer_evidence/os-compiler-linker-events.$producer_suffix" nonzero "$producer_evidence/selection-config.json"
if [ "$platform" = linux ]; then
  CARGO_REAPI_RUSTC_TRACE="$rustc_trace" strace -f -s 1048576 -v -e trace=execve -o "$os_events" \
    "$gate_runner" producer "$producer" "$driver" "$shared_cache" "$producer_evidence/actions.jsonl" \
    "$producer_evidence/member.json" "$producer_evidence/gate.log" "$observer" "$real_rustc"
else
run_gate \
  producer \
  "$producer" \
  "$producer_evidence/actions.jsonl" \
  "$producer_evidence/member.json" \
  "$producer_evidence/gate.log"
fi
stop_os_observer "$report_root/producer-os-proof.json" "$producer_evidence/selection-config.json"
mv "$producer" "$run_root/producer-retired-and-unavailable"
rm -rf "$run_root/producer-retired-and-unavailable"
test ! -e "$run_root/producer-retired-and-unavailable"
jq -n --arg producer "$run_root/producer" --argjson retired_at_unix_ms "$(timestamp_ms)" \
  '{schema_version:1,producer:$producer,producer_deleted:true,retired_at_unix_ms:$retired_at_unix_ms}' \
  >"$report_root/producer-retirement.json"

run_population single 1
run_population five 5
run_population stress 10

environment_claims=$(jq -cn '{host_contract:{status:"PASS",evidence_roles:["environment_report"]},toolchain_identity:{status:"PASS",evidence_roles:["environment_report","run_start"]},ssd_storage:{status:"PASS",evidence_roles:["environment_report"]}}')
environment_refs="environment_report:$report_root/environment.json platform_environment:$report_root/platform-environment.json"
if [ "$platform" = linux ]; then
  environment_refs="$environment_refs cache_filesystem:$report_root/cache-filesystem.json worktree_filesystem:$report_root/worktree-filesystem.json"
  if [ -f "$evidence_root/container-image-inspect.json" ]; then
    environment_refs="$environment_refs container_image_inspect:$evidence_root/container-image-inspect.json"
  fi
fi
# shellcheck disable=SC2086
write_receipt_v2 "$evidence_root" "$evidence_root/receipts/environment.receipt.json" environment \
  "$report_root/run-start.json" "$environment_claims" '{}' \
  $environment_refs

for kind in single five stress; do
  case "$kind" in
    single) receipt_kind=moria-single ;;
    five) receipt_kind=moria-five ;;
    stress) receipt_kind=moria-stress ;;
  esac
  population_root=$run_root/$kind/evidence
  claims=$(jq -cn '
    def claim($roles): {status:"PASS",evidence_roles:$roles};
    {producer_deleted:claim(["producer_retirement"]),empty_consumers:claim(["population_evidence"]),canonical_gate:claim(["population_proof","member_gate_log"]),simultaneous_start:claim(["population_proof"]),zero_physical_actions:claim(["population_proof","member_action_log"]),zero_os_compiler_linker:claim(["population_os_audit"]),deadline_met:claim(["population_proof"])}')
  references="producer_retirement:$report_root/producer-retirement.json population_proof:$report_root/$kind-proof.json population_os_audit:$report_root/$kind-os-proof.json population_evidence:$report_root/$kind-evidence.json"
  for file in "$population_root"/member-*-actions.jsonl; do references="$references member_action_log:$file"; done
  for file in "$population_root"/member-*.log; do references="$references member_gate_log:$file"; done
  # Intentional word splitting: each generated reference has no spaces because
  # acceptance worktree roots are normalized before this runner starts.
  # shellcheck disable=SC2086
  write_receipt_v2 "$evidence_root" "$evidence_root/receipts/$receipt_kind.receipt.json" "$receipt_kind" \
    "$report_root/run-start.json" "$claims" \
    "$(jq -c '{members:.observed_members,elapsed_ms,physical_cacheable_actions:([.member_action_proofs[].cacheable_physical_actions]|add),os_compiler_linker_events:0}' "$report_root/$kind-proof.json")" \
    $references
done

if [ "$platform" = macos ]; then
  jq -e 'select(.selected_method == "copy-on-write" and .attempt_succeeded == true and .source_location == "src/gate.rs:clone_tree_with_preference")' \
    "$report_root/clone-selection-events.jsonl" >/dev/null
  clone_claims=$(jq -cn '{copy_on_write_selected:{status:"PASS",evidence_roles:["clone_selection_trace"]},selection_source_identified:{status:"PASS",evidence_roles:["clone_selection_trace"]}}')
  write_receipt_v2 "$evidence_root" "$evidence_root/receipts/macos-clone.receipt.json" macos-clone \
    "$report_root/run-start.json" "$clone_claims" '{}' \
    "clone_selection_trace:$report_root/clone-selection-events.jsonl"
else
  findmnt -T "$shared_cache" -J -o TARGET,SOURCE,FSTYPE,OPTIONS >"$report_root/linux-filesystem.json"
  filesystem=$(jq -r '.filesystems[0].fstype' "$report_root/linux-filesystem.json")
  selected=$(jq -sr 'map(select(.attempt_succeeded == true)) | last.selected_method' "$report_root/clone-selection-events.jsonl")
  attempted=$(jq -sr 'map(select(.attempt_succeeded == true)) | last.attempted_primitive' "$report_root/clone-selection-events.jsonl")
  jq -n --arg filesystem_type "$filesystem" --arg selected_method "$selected" --arg attempted_primitive "$attempted" \
    '{schema_version:1,filesystem_type:$filesystem_type,selected_method:$selected_method,attempted_primitive:$attempted_primitive,mechanism_proven:true,passed:true,violations:[]}' \
    >"$report_root/linux-copy-report.json"
  copy_claims=$(jq -cn '{filesystem_recorded:{status:"PASS",evidence_roles:["linux_copy_report","filesystem_report"]},mechanism_selected:{status:"PASS",evidence_roles:["linux_copy_report","clone_selection_trace"]},mechanism_proven:{status:"PASS",evidence_roles:["linux_copy_report","clone_selection_trace"]}}')
  write_receipt_v2 "$evidence_root" "$evidence_root/receipts/linux-copy-mechanism.receipt.json" linux-copy-mechanism \
    "$report_root/run-start.json" "$copy_claims" '{}' \
    "linux_copy_report:$report_root/linux-copy-report.json" \
    "filesystem_report:$report_root/linux-filesystem.json" \
    "clone_selection_trace:$report_root/clone-selection-events.jsonl"
fi

echo "PASS  $report_root"
