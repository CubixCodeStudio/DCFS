#!/usr/bin/env bash
# Take the server away in the middle of a copy and put it back. The copy has to
# finish with the right bytes: an application writing to a mount cannot retry
# for itself, so a moment without the server must look like a slow write.
#
# Linux only: needs /dev/fuse and fuse3. Starts its own server, so set
# DATABASE_URL to a THROWAWAY database.
#
#   DATABASE_URL=postgresql://... scripts/fuse-outage.sh
set -uo pipefail

if [[ ! -e /dev/fuse ]]; then
    echo "SKIP: /dev/fuse is missing; this needs Linux with fuse3." >&2
    exit 0
fi
export MASTER_KEY=$(openssl rand -hex 32) API_TOKEN=$(openssl rand -hex 32)
export SERVER_ADDR=127.0.0.1:8080 RUST_LOG=warn OBJECT_STORE_PATH=$(mktemp -d) GC_RETENTION_SECS=3600
export DATABASE_AUTO_MIGRATE=true
# PROFILE=release for a realistic copy speed; debug works but takes longer.
PROFILE="${PROFILE:-release}"
[ "$PROFILE" = release ] && flag=--release || flag=
cargo build -q $flag --bin discordfs-server --bin discordfs-fuse || exit 1
bin="${CARGO_TARGET_DIR:-target}/$PROFILE"
mnt=$(mktemp -d); work=$(mktemp -d)

# The test restarts the server, so cleaning up by one recorded pid is not
# enough: leaving one behind holds the port against whatever runs next.
cleanup() {
    fusermount3 -u "$mnt" 2>/dev/null || true
    pkill -f "$bin/discordfs-server" 2>/dev/null || true
    rm -rf "$mnt" "$work" "$OBJECT_STORE_PATH"
}
trap cleanup EXIT

start_server() { "$bin/discordfs-server" >> /tmp/server.log 2>&1 & echo $!; }
pid=$(start_server)
for _ in $(seq 1 60); do curl -sf http://$SERVER_ADDR/health >/dev/null && break; sleep 1; done

RUST_LOG=discordfs_fuse=warn DISCORDFS_TOKEN=$API_TOKEN "$bin/discordfs-fuse" "$mnt" \
    --server http://$SERVER_ADDR > /tmp/fuse.log 2>&1 &
for _ in $(seq 1 30); do mountpoint -q "$mnt" && break; sleep 1; done
mountpoint -q "$mnt" || { echo "not mounted"; cat /tmp/fuse.log; exit 1; }
echo "mounted"

head -c 31457280 /dev/urandom > "$work/src.bin"   # 30 MB
want=$(sha256sum < "$work/src.bin" | cut -d' ' -f1)

# Kill the server a moment into the copy, bring it back shortly after.
( sleep 1.2; echo "  (server down)"; kill "$pid"; sleep 2.5; start_server >/dev/null; echo "  (server back)" ) &
disruptor=$!

cp "$work/src.bin" "$mnt/through-outage.bin" 2>/tmp/cp.err
rc=$?
wait "$disruptor" 2>/dev/null

got=$(sha256sum < "$mnt/through-outage.bin" 2>/dev/null | cut -d' ' -f1)
size=$(stat -c %s "$mnt/through-outage.bin" 2>/dev/null)
echo "retries logged by the client: $(grep -c 'retrying' /tmp/fuse.log)"

if [ "$rc" = 0 ] && [ "$got" = "$want" ]; then
    echo "PASS  30 MB copy survived the server restarting mid-copy (size $size)"
    exit 0
fi
echo "FAIL  cp_exit=$rc size=$size err=$(cat /tmp/cp.err)" >&2
echo "      want ${want:0:16} got ${got:0:16}" >&2
exit 1
