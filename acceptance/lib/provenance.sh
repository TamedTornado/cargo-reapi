#!/bin/sh

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

timestamp_ms() {
  perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'
}

write_intrinsic_run_start() {
  report_root=$1
  runner=$2
  driver=$3
  auditor=$4
  profile=$5
  batch_id=$6
  repo=$(cd "$(dirname "$runner")/.." && pwd)
  runner_rel=${runner#$repo/}
  criteria=$repo/acceptance/ACCEPTANCE_CRITERIA.md
  contract=$repo/acceptance/contract.toml
  mkdir -p "$report_root"
  final=$report_root/run-start.json
  test ! -e "$final"
  git -C "$repo" diff --quiet -- Cargo.toml Cargo.lock src acceptance
  git -C "$repo" diff --cached --quiet -- Cargo.toml Cargo.lock src acceptance
  runner_sha=$(sha256_file "$runner")
  criteria_sha=$(sha256_file "$criteria")
  criteria_blob=$(git -C "$repo" hash-object "$criteria")
  criteria_commit=$(git -C "$repo" log -1 --format=%H -- "$criteria")
  source_revision=$(git -C "$repo" rev-parse HEAD)
  implementation_sha=$(
    git -C "$repo" ls-tree -r HEAD -- Cargo.toml Cargo.lock src acceptance |
      sha256_stream
  )
  started=$(timestamp_ms)
  temporary=$report_root/.run-start.$$.json
  jq -n \
    --arg runner_path "$runner_rel" \
    --arg runner_sha256 "$runner_sha" \
    --arg criteria_sha256 "$criteria_sha" \
    --arg criteria_git_blob "$criteria_blob" \
    --arg criteria_commit "$criteria_commit" \
    --arg contract_sha256 "$(sha256_file "$contract")" \
    --arg source_revision "$source_revision" \
    --arg implementation_tree_sha256 "$implementation_sha" \
    --arg driver_sha256 "$(sha256_file "$driver")" \
    --arg auditor_sha256 "$(sha256_file "$auditor")" \
    --arg cargo_version "$(cargo --version)" \
    --arg rustc_version "$(rustc --version --verbose)" \
    --arg platform_profile_sha256 "$(sha256_file "$profile")" \
    --arg platform_os "$(rustc -vV | awk '/^host:/{print $2}' | awk -F- '{if ($3=="darwin") print "macos"; else if ($3=="linux" || $4=="linux") print "linux"; else print $3}')" \
    --arg platform_arch "$(rustc -vV | awk '/^host:/{print $2}' | cut -d- -f1)" \
    --arg batch_id "$batch_id" \
    --argjson started_at_unix_ms "$started" \
    '{schema_version:2,harness_identity:"intrinsic",runner_path:$runner_path,runner_sha256:$runner_sha256,criteria_sha256:$criteria_sha256,criteria_git_blob:$criteria_git_blob,criteria_commit:$criteria_commit,contract_sha256:$contract_sha256,source_revision:$source_revision,implementation_tree_sha256:$implementation_tree_sha256,driver_sha256:$driver_sha256,auditor_sha256:$auditor_sha256,cargo_version:$cargo_version,rustc_version:$rustc_version,platform_profile_sha256:$platform_profile_sha256,platform_os:$platform_os,platform_arch:$platform_arch,batch_id:$batch_id,started_at_unix_ms:$started_at_unix_ms}' \
    >"$temporary"
  sync "$temporary" 2>/dev/null || true
  ln "$temporary" "$final"
  rm "$temporary"
}

write_receipt_v2() {
  evidence_root=$1
  receipt=$2
  kind=$3
  run_start=$4
  claims=$5
  measurements=$6
  shift 6
  refs='[]'
  add_ref() {
    role=${1%%:*}
    path=${1#*:}
    test -f "$path"
    case "$path" in
      "$evidence_root"/*) relative=${path#$evidence_root/} ;;
      *) echo "evidence is outside sealed root: $path" >&2; exit 2 ;;
    esac
    digest=$(sha256_file "$path")
    refs=$(printf '%s\n' "$refs" | jq -c --arg role "$role" --arg path "$relative" --arg sha256 "$digest" '. + [{role:$role,path:$path,sha256:$sha256}]')
  }
  add_ref "run_start:$run_start"
  for reference in "$@"; do
    add_ref "$reference"
  done
  identity=$(jq '{contract_sha256,criteria_sha256,implementation_tree_sha256,source_revision,driver_sha256,auditor_sha256,cargo_version,rustc_version,platform_profile_sha256,platform_os,platform_arch,batch_id}' "$run_start")
  provenance=$(jq '{harness_identity,runner_path,runner_sha256,criteria_sha256,criteria_git_blob,criteria_commit,started_at_unix_ms}' "$run_start")
  mkdir -p "$(dirname "$receipt")"
  jq -n \
    --arg kind "$kind" \
    --argjson identity "$identity" \
    --argjson provenance "$provenance" \
    --argjson refs "$refs" \
    --argjson claims "$claims" \
    --argjson measurements "$measurements" \
    '{schema_version:2,kind:$kind,status:"PASS",identity:$identity,provenance:$provenance,evidence_refs:$refs,claims:$claims,measurements:$measurements,violations:[]}' \
    >"$receipt"
}

write_platform_batch_v2() {
  evidence_root=$1
  platform_id=$2
  run_start=$3
  shift 3
  receipts='{}'
  for kind in "$@"; do
    path=$evidence_root/receipts/$kind.receipt.json
    test -f "$path"
    digest=$(sha256_file "$path")
    receipts=$(printf '%s\n' "$receipts" | jq -c --arg kind "$kind" --arg path "receipts/$kind.receipt.json" --arg sha256 "$digest" '. + {($kind):{path:$path,sha256:$sha256}}')
  done
  identity=$(jq '{contract_sha256,criteria_sha256,implementation_tree_sha256,source_revision,driver_sha256,auditor_sha256,cargo_version,rustc_version,platform_profile_sha256,platform_os,platform_arch,batch_id}' "$run_start")
  started=$(jq '.started_at_unix_ms' "$run_start")
  completed=$(timestamp_ms)
  jq -n \
    --arg platform_id "$platform_id" \
    --argjson identity "$identity" \
    --argjson started "$started" \
    --argjson completed "$completed" \
    --argjson receipts "$receipts" \
    '{schema_version:2,platform_id:$platform_id,status:"PASS",identity:$identity,started_at_unix_ms:$started,completed_at_unix_ms:$completed,receipts:$receipts,violations:[]}' \
    >"$evidence_root/batch.json"
}

sha256_stream() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}
