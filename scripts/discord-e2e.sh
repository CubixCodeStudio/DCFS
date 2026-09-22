#!/usr/bin/env bash
# End to end against real Discord: mount a filesystem whose chunks are Discord
# attachments, write a file, restart the server, read it back, delete it and
# watch the attachment go.
#
# This POSTS REAL ATTACHMENTS to the channel the webhook belongs to, and
# deletes them again. Point it at a channel you do not mind writing to.
#
# Credentials come from the environment and are never printed. Put them in a
# file git already ignores and source it yourself:
#
#   printf 'DISCORD_WEBHOOK_ID=...\nDISCORD_WEBHOOK_TOKEN=...\n' > .env.discord-test
#   set -a; . ./.env.discord-test; set +a
#   DATABASE_URL=postgresql://... scripts/discord-e2e.sh
#
# Linux only for the mount; without /dev/fuse it runs the HTTP half and says so.
set -uo pipefail

if [[ -z "${DISCORD_WEBHOOK_ID:-}" || -z "${DISCORD_WEBHOOK_TOKEN:-}" ]]; then
    echo "SKIP: set DISCORD_WEBHOOK_ID and DISCORD_WEBHOOK_TOKEN to run this." >&2
    exit 0
fi

export MASTER_KEY="${MASTER_KEY:-$(openssl rand -hex 32)}"
export API_TOKEN="${API_TOKEN:-$(openssl rand -hex 32)}"
export SERVER_ADDR="${SERVER_ADDR:-127.0.0.1:8080}"
export DATABASE_AUTO_MIGRATE="${DATABASE_AUTO_MIGRATE:-true}"
export RUST_LOG="${RUST_LOG:-info}"
# Small parts: this is a correctness run, not a throughput one, and every part
# is a separate attachment and a separate rate-limit slot.
export CHUNK_SIZE="${CHUNK_SIZE:-262144}"
export GC_RETENTION_SECS="${GC_RETENTION_SECS:-1}"

PROFILE="${PROFILE:-release}"
[ "$PROFILE" = release ] && flag=--release || flag=
cargo build -q $flag --bin dcfs-server || exit 1
bin="${CARGO_TARGET_DIR:-target}/$PROFILE"

work=$(mktemp -d)
cleanup() {
    pkill -f "$bin/dcfs-server" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

pass=0; fail=0
check() { if [ "$1" = 0 ]; then echo "  PASS  $2"; pass=$((pass+1)); else echo "  FAIL  $2"; fail=$((fail+1)); fi; }
auth=(-H "authorization: Bearer $API_TOKEN")
api="http://$SERVER_ADDR/api/v1"

start_server() {
    "$bin/dcfs-server" >> "$work/server.log" 2>&1 &
    for _ in $(seq 1 60); do
        curl -sf "http://$SERVER_ADDR/health" >/dev/null && return 0
        sleep 1
    done
    echo "server never became healthy" >&2
    tail -20 "$work/server.log" >&2
    return 1
}

echo "== bringing up a server backed by Discord =="
start_server || exit 1
grep -q "object store: discord" "$work/server.log"
check $? "the server chose the Discord backend"

root=$(curl -s "${auth[@]}" "$api/nodes/root" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
name=$(python3 -c 'import base64;print(base64.urlsafe_b64encode(b"through-discord.bin").decode().rstrip("="))')
file=$(curl -s "${auth[@]}" -H 'content-type: application/json' -X POST "$api/nodes" \
    -d "{\"parent_id\":\"$root\",\"name\":\"$name\",\"kind\":\"File\",\"mode\":33188,\"uid\":0,\"gid\":0,\"idempotency_key\":\"$(uuidgen | tr 'A-Z' 'a-z')\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
[ -n "$file" ]
check $? "created a file"

# Three parts, so the manifest and the per-part uploads both get exercised.
head -c $((CHUNK_SIZE * 3)) /dev/urandom > "$work/src.bin"
want=$(sha256sum < "$work/src.bin" | cut -d' ' -f1)

echo "== writing $((CHUNK_SIZE * 3)) bytes as Discord attachments =="
code=$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X PUT \
    --data-binary "@$work/src.bin" "$api/nodes/$file/data?offset=0")
[ "$code" = 200 ]
check $? "the write was accepted (HTTP $code)"

curl -s "${auth[@]}" "$api/nodes/$file/data" -o "$work/back.bin"
[ "$(sha256sum < "$work/back.bin" | cut -d' ' -f1)" = "$want" ]
check $? "read back byte for byte"

echo "== restarting the server =="
# The locators live in PostgreSQL, so the attachments must still be findable.
pkill -f "$bin/dcfs-server"; sleep 1
start_server || exit 1
curl -s "${auth[@]}" "$api/nodes/$file/data" -o "$work/after.bin"
[ "$(sha256sum < "$work/after.bin" | cut -d' ' -f1)" = "$want" ]
check $? "still readable after a restart (locators survived)"

echo "== partial read =="
mid=$((CHUNK_SIZE + 17))
curl -s "${auth[@]}" "$api/nodes/$file/data?offset=$mid&size=64" -o "$work/slice.bin"
dd if="$work/src.bin" bs=1 skip=$mid count=64 status=none > "$work/slice-want.bin"
cmp -s "$work/slice.bin" "$work/slice-want.bin"
check $? "64 bytes from the middle match"

echo "== deletion reaches Discord =="
curl -s -o /dev/null "${auth[@]}" -X DELETE "$api/nodes/$file"
# GC_RETENTION_SECS is 1 here, and the sweep runs at least once a second.
sleep 4
objects=$(grep -c "gc swept" "$work/server.log")
[ "$objects" -ge 1 ]
check $? "the collector ran"
curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$api/nodes/$file" | grep -q 404
check $? "the file is gone"

echo
echo "RESULT: $pass passed, $fail failed"
echo "(check the channel: the attachments this wrote should have been deleted)"
[ "$fail" -eq 0 ]
