#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 SOURCE_ROOT DATA_ROOT" >&2
  exit 2
fi

source_root=$(cd "$1" && pwd)
mkdir -p "$2"
data_root=$(cd "$2" && pwd)
cargo_reapi=$source_root/cargo-reapi
bro=$source_root/bro
moria=$source_root/moria
for repository in "$cargo_reapi" "$bro" "$moria"; do test -d "$repository/.git"; done

image=cargo-reapi-linux-qualification:rust-1.97.1-node-22
docker build --pull=false -t "$image" -f "$cargo_reapi/acceptance/linux/Dockerfile" "$cargo_reapi"
run_id=$(date -u +%Y%m%dT%H%M%SZ)
run_root=$data_root/$run_id
mkdir -p "$run_root/cache" "$run_root/evidence"
docker image inspect "$image" >"$run_root/evidence/container-image-inspect.json"

docker run --rm --privileged --security-opt seccomp=unconfined \
  --mount "type=bind,src=$source_root,dst=/work" \
  --mount "type=bind,src=$run_root,dst=/qualification" \
  "$image" bash -lc '
    set -euo pipefail
    export PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
    cd /work/bro
    pnpm install --frozen-lockfile
    cd /work/cargo-reapi
    cargo fetch --locked
    (cd /work/moria && cargo fetch --locked)
    (cd /work/cargo-reapi/acceptance/bevy-fixture && cargo fetch --locked)
    cargo build --release --bins
    acceptance/run-platform-qualification.sh \
      /work/moria /work/bro /qualification/cache /qualification/evidence ssd
  '

echo "PASS  $run_root/evidence"
