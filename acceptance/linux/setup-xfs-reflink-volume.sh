#!/bin/sh
set -eu

image=${1:-/home/chandler/cargo-reapi-qualification-xfs.img}
mountpoint=${2:-/home/chandler/cargo-reapi-qualification-xfs}
size=${3:-260G}

case "$image:$mountpoint" in
  /*:/*) ;;
  *)
    echo "image and mountpoint must be absolute paths" >&2
    exit 2
    ;;
esac

if [ "$(id -u)" -ne 0 ]; then
  echo "usage: sudo $0 [IMAGE [MOUNTPOINT [SIZE]]]" >&2
  exit 2
fi

if mountpoint -q "$mountpoint"; then
  echo "already mounted: $mountpoint"
  xfs_info "$mountpoint"
  exit 0
fi

if [ -e "$image" ]; then
  filesystem=$(blkid -s TYPE -o value "$image" 2>/dev/null || true)
  if [ "$filesystem" != xfs ]; then
    echo "refusing to format existing non-XFS path: $image" >&2
    exit 1
  fi
else
  truncate -s "$size" "$image"
  mkfs.xfs -f -m reflink=1 "$image"
fi

mkdir -p "$mountpoint"
mount -o loop,noatime "$image" "$mountpoint"
owner_uid=${SUDO_UID:-1000}
owner_gid=${SUDO_GID:-1000}
chown "$owner_uid:$owner_gid" "$mountpoint"

xfs_info "$mountpoint" | tee "$mountpoint/xfs-info.txt"
grep -q 'reflink=1' "$mountpoint/xfs-info.txt"
echo "XFS reflink qualification volume ready: $mountpoint"
