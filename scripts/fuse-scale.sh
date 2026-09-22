#!/usr/bin/env bash
# How the cost of a write grows with the size of the file.
#
# Writes several sizes through a real mount and reports wall time, throughput
# and what each one left in the metadata store. The point is the shape of the
# curve, not any single number: a per-write cost that grows with the file is
# what stops a filesystem scaling to very large files.
#
# No source file is kept on disk — the bytes come from a keystream that can be
# regenerated to check them — so the only space needed is for the objects.
#
#   DATABASE_URL=postgresql://... SIZES="256 512 1024" scripts/fuse-scale.sh
set -uo pipefail

if [[ ! -e /dev/fuse ]]; then
    echo "SKIP: /dev/fuse is missing; this needs Linux with fuse3." >&2
    exit 0
fi

export MASTER_KEY="${MASTER_KEY:-$(openssl rand -hex 32)}"
export API_TOKEN="${API_TOKEN:-$(openssl rand -hex 32)}"
export SERVER_ADDR="${SERVER_ADDR:-127.0.0.1:8080}"
export OBJECT_STORE_PATH="${OBJECT_STORE_PATH:-$(mktemp -d)}"
export CHUNK_SIZE="${CHUNK_SIZE:-16777216}"
# Nothing is deleted here, so keep the collector out of the measurements.
export GC_RETENTION_SECS="${GC_RETENTION_SECS:-86400}"
export RUST_LOG="${RUST_LOG:-warn}"

PROFILE="${PROFILE:-release}"
[ "$PROFILE" = release ] && flag=--release || flag=
cargo build -q $flag --bin discordfs-server --bin discordfs-fuse || exit 1
bin="${CARGO_TARGET_DIR:-target}/$PROFILE"

mnt=$(mktemp -d)
cleanup() {
    fusermount3 -u "$mnt" 2>/dev/null || true
    pkill -f "$bin/discordfs-server" 2>/dev/null || true
    rm -rf "$mnt" "$OBJECT_STORE_PATH"
}
trap cleanup EXIT

"$bin/discordfs-server" > /tmp/scale-server.log 2>&1 &
for _ in $(seq 1 60); do curl -sf "http://$SERVER_ADDR/health" >/dev/null && break; sleep 1; done
DISCORDFS_TOKEN=$API_TOKEN "$bin/discordfs-fuse" "$mnt" --server "http://$SERVER_ADDR" \
    > /tmp/scale-fuse.log 2>&1 &
for _ in $(seq 1 30); do mountpoint -q "$mnt" && break; sleep 1; done
mountpoint -q "$mnt" || { echo "not mounted" >&2; exit 1; }

# A deterministic keystream: the same bytes every time, stored nowhere.
stream() { head -c "$1" /dev/zero | openssl enc -aes-256-ctr -K "$(printf '%064d' 7)" -iv "$(printf '%032d' 7)" 2>/dev/null; }

printf '%8s  %9s  %9s  %8s  %7s  %9s  %6s\n' \
    SIZE WRITE READ MB/s PARTS "CHUNKROWS" VERS
for mb in ${SIZES:-256 512 1024}; do
    bytes=$((mb * 1024 * 1024))
    avail=$(df -k "$OBJECT_STORE_PATH" | awk 'NR==2{print $4 * 1024}')
    if [ "$avail" -lt $((bytes + 536870912)) ]; then
        printf '%8s  %s\n' "${mb}M" "skipped: needs $((bytes / 1048576)) MiB, $((avail / 1048576)) MiB free"
        continue
    fi

    t0=$(date +%s.%N)
    stream "$bytes" > "$mnt/scale-$mb.bin"
    t1=$(date +%s.%N)
    got=$(sha256sum < "$mnt/scale-$mb.bin" | cut -d' ' -f1)
    t2=$(date +%s.%N)
    want=$(stream "$bytes" | sha256sum | cut -d' ' -f1)

    write=$(echo "$t0 $t1" | awk '{printf "%.1f", $2-$1}')
    read=$(echo "$t1 $t2" | awk '{printf "%.1f", $2-$1}')
    rate=$(echo "$mb $write" | awk '{printf "%.1f", $1/$2}')
    parts=$(find "$OBJECT_STORE_PATH" -type f | wc -l | tr -d ' ')
    if [ -n "${DATABASE_URL:-}" ] && command -v psql >/dev/null; then
        rows=$(psql "$DATABASE_URL" -tAc 'SELECT count(*) FROM file_chunks' 2>/dev/null | tr -d ' ')
        vers=$(psql "$DATABASE_URL" -tAc 'SELECT count(*) FROM file_versions' 2>/dev/null | tr -d ' ')
    else
        rows=-; vers=-
    fi
    status=ok; [ "$got" = "$want" ] || status=CORRUPT
    printf '%8s  %8ss  %8ss  %8s  %7s  %9s  %6s  %s\n' \
        "${mb}M" "$write" "$read" "$rate" "$parts" "${rows:--}" "${vers:--}" "$status"
    [ "$status" = ok ] || exit 1
done
