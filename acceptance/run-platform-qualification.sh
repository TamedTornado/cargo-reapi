#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: $0 MORIA_ROOT BRO_ROOT CACHE_DIR EVIDENCE_ROOT STORAGE_PROFILE" >&2
  exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
moria_root=$(cd "$1" && pwd)
bro_root=$(cd "$2" && pwd)
mkdir -p "$3" "$4"
cache_dir=$(cd "$3" && pwd)
evidence_root=$(cd "$4" && pwd)
storage_profile=$5
case "$(uname -s)" in
  Darwin) platform_id=macos-arm64; profile=$repo/acceptance/platforms/macos-arm64.toml ;;
  Linux) platform_id=linux-x86_64; profile=$repo/acceptance/platforms/linux-x86_64.toml ;;
  *) echo "unsupported acceptance platform" >&2; exit 2 ;;
esac
batch_id="cargo-reapi-$platform_id-$(date -u +%Y%m%dT%H%M%SZ)"

driver=$repo/target/release/cargo-reapi
exec_auditor=$repo/target/release/cargo-reapi-exec-auditor
resource_auditor=$repo/target/release/cargo-reapi-auditor
test -x "$driver" -a -x "$exec_auditor" -a -x "$resource_auditor"

. "$repo/acceptance/lib/provenance.sh"
jq -n \
  --arg cargo_reapi "$(sha256_file "$driver")" \
  --arg exec_auditor "$(sha256_file "$exec_auditor")" \
  --arg resource_auditor "$(sha256_file "$resource_auditor")" \
  '{schema_version:1,cargo_reapi_sha256:$cargo_reapi,exec_auditor_sha256:$exec_auditor,resource_auditor_sha256:$resource_auditor}' \
  >"$evidence_root/auditor-bundle.json"
export CARGO_REAPI_EVIDENCE_ROOT=$evidence_root
export CARGO_REAPI_AUDITOR_IDENTITY=$evidence_root/auditor-bundle.json
export CARGO_REAPI_BATCH_ID=$batch_id

cp "$profile" "$evidence_root/platform.toml"
mkdir -p "$evidence_root/receipts" "$evidence_root/moria" "$evidence_root/adversarial" \
  "$evidence_root/bevy" "$evidence_root/bro" "$evidence_root/resources"

"$resource_auditor" run \
  --report "$evidence_root/resource-build-report.json" \
  --ledger-root "$cache_dir/resource-ledger-v1" \
  --stall-seconds 300 -- \
  "$repo/acceptance/run-moria-local.sh" "$moria_root" "$cache_dir" "$evidence_root/moria" "$storage_profile"

"$repo/acceptance/run-adversarial.sh" "$evidence_root/adversarial" "$batch_id"
"$repo/acceptance/run-bevy-integrity.sh" "$evidence_root/bevy" "$batch_id"
"$repo/acceptance/run-bro-five.sh" "$evidence_root/bro" "$batch_id" "$bro_root" "$moria_root" "$cache_dir" \
  "$evidence_root/moria/producer-retirement.json" "$storage_profile"
"$repo/acceptance/run-resources.sh" "$evidence_root/resources" "$batch_id" \
  "$evidence_root/resource-build-report.json" "$cache_dir/resource-ledger-v1" "$evidence_root/stall-ledger"

if [ "$platform_id" = macos-arm64 ]; then
  copy_kind=macos-clone
else
  copy_kind=linux-copy-mechanism
fi
write_platform_batch_v2 "$evidence_root" "$platform_id" "$evidence_root/moria/run-start.json" \
  environment adversarial bevy-integrity coalescing resources portable-copy-isolated "$copy_kind" \
  moria-single moria-five moria-stress bro-five

echo "PASS  $evidence_root/batch.json"
