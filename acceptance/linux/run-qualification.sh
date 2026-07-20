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
mkdir -p "$run_root/cache" "$run_root/evidence" "$run_root/cargo-registry" "$run_root/cargo-git"
docker image inspect "$image" >"$run_root/evidence/container-image-inspect.json"

# Provision dependencies with network access before the qualification boundary
# is created. The acceptance workload below receives these stores read-only.
docker run --rm --env HOME=/home/qualifier \
  --mount "type=bind,src=$source_root,dst=/work" \
  --mount "type=bind,src=$run_root/cargo-registry,dst=/usr/local/cargo/registry" \
  --mount "type=bind,src=$run_root/cargo-git,dst=/usr/local/cargo/git" \
  "$image" bash -lc '
    set -euo pipefail
    export PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
    git config --global --add safe.directory /work/cargo-reapi
    git config --global --add safe.directory /work/bro
    git config --global --add safe.directory /work/moria
    cd /work/bro
    pnpm install --frozen-lockfile
    cd /work/cargo-reapi
    cargo fetch --locked
    (cd /work/moria && cargo fetch --locked)
    (cd /work/cargo-reapi/acceptance/bevy-fixture && cargo fetch --locked)
    cargo build --release --bins
  '

userns_policy=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
original_userns_policy=$(cat "$userns_policy")
printf '%s\n' "$original_userns_policy" >"$run_root/evidence/host-userns-policy-before.txt"
restore_userns_policy() {
  docker run --rm --privileged --security-opt systempaths=unconfined --user 0:0 "$image" \
    sh -c "printf '%s\\n' '$original_userns_policy' > '$userns_policy'"
  cat "$userns_policy" >"$run_root/evidence/host-userns-policy-after.txt"
}
qualification_client_pid=
qualification_container_id=
cleanup() {
  if [ -n "$qualification_container_id" ]; then
    docker stop "$qualification_container_id" >/dev/null 2>&1 || true
  fi
  if [ -n "$qualification_client_pid" ]; then
    kill "$qualification_client_pid" 2>/dev/null || true
    wait "$qualification_client_pid" 2>/dev/null || true
  fi
  restore_userns_policy
}
trap cleanup EXIT HUP INT TERM
if [ "$original_userns_policy" != 0 ]; then
  docker run --rm --privileged --security-opt systempaths=unconfined --user 0:0 "$image" \
    sh -c "printf '0\\n' > '$userns_policy'"
fi
cat "$userns_policy" >"$run_root/evidence/host-userns-policy-during.txt"
test "$(cat "$run_root/evidence/host-userns-policy-during.txt")" = 0

docker run --rm --cidfile "$run_root/qualification.cid" --network none --cap-drop ALL \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  --security-opt no-new-privileges=true \
  --env HOME=/home/qualifier \
  --mount "type=bind,src=$source_root,dst=/work,readonly" \
  --mount "type=bind,src=$cargo_reapi/target,dst=/work/cargo-reapi/target" \
  --mount "type=bind,src=$run_root,dst=/qualification" \
  --mount "type=bind,src=$run_root/cargo-registry,dst=/usr/local/cargo/registry,readonly" \
  --mount "type=bind,src=$run_root/cargo-git,dst=/usr/local/cargo/git,readonly" \
  "$image" bash -lc '
    set -euo pipefail
    export PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
    export CARGO_NET_OFFLINE=true
    git config --global --add safe.directory /work/cargo-reapi
    git config --global --add safe.directory /work/bro
    git config --global --add safe.directory /work/moria
    cd /work/cargo-reapi
    acceptance/run-platform-qualification.sh \
      /work/moria /work/bro /qualification/cache /qualification/evidence ssd
  ' &
qualification_client_pid=$!
attempt=0
while [ ! -s "$run_root/qualification.cid" ] && kill -0 "$qualification_client_pid" 2>/dev/null && [ "$attempt" -lt 30 ]; do
  sleep 1
  attempt=$((attempt + 1))
done
test -s "$run_root/qualification.cid"
qualification_container_id=$(cat "$run_root/qualification.cid")
docker inspect "$qualification_container_id" >"$run_root/evidence/qualification-container-inspect.json"
wait "$qualification_client_pid"
qualification_client_pid=
qualification_container_id=

restore_userns_policy
trap - EXIT HUP INT TERM

echo "PASS  $run_root/evidence"
