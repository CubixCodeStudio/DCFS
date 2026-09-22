#!/usr/bin/env bash
# Does refreshing a Discord message after its CDN link expires yield a working one?
#
# The whole refresh path rests on this, and it cannot be tested in under a day:
# within the validity window Discord hands back the identical signed URL. So an
# object is uploaded, left alone, and checked once its link is past `ex`.
#
# Reads .env.expiry-probe.json, written when the probe was uploaded.
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f .env.expiry-probe.json ] || { echo "no probe recorded; nothing to check" >&2; exit 1; }
set -a; . ./.env.development; set +a
HOOK="https://discord.com/api/v10/webhooks/${DISCORD_WEBHOOK_ID}/${DISCORD_WEBHOOK_TOKEN}"

read -r MSG OLD_URL WANT_SHA EXPIRES <<<"$(python3 -c '
import json
r = json.load(open(".env.expiry-probe.json"))
print(r["message_id"], r["original_url"], r["sha256"], r["expires_at_utc"])
')"

python3 - "$EXPIRES" <<'PY'
import datetime, sys
expiry = datetime.datetime.fromisoformat(sys.argv[1])
left = (expiry - datetime.datetime.now(datetime.UTC)).total_seconds()
if left > 0:
    raise SystemExit(f"link is still valid for {left/3600:.1f}h; check after {expiry}")
print(f"link expired {-left/3600:.1f}h ago — testing")
PY

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

echo "1. ยิง URL เดิมที่หมดอายุแล้ว"
OLD_CODE=$(curl -s -o "$TMP/old" -w '%{http_code}' "$OLD_URL")
echo "   -> HTTP $OLD_CODE $([ "$OLD_CODE" = 200 ] && echo '(ยังใช้ได้ — ไม่ได้บังคับใช้วันหมดอายุ)' || echo '(ตายแล้ว ตามคาด)')"

echo "2. refresh จาก message"
curl -s -o "$TMP/msg" "$HOOK/messages/$MSG"
NEW_URL=$(python3 -c '
import json, sys
print(json.load(open(sys.argv[1]))["attachments"][0]["url"])' "$TMP/msg")
python3 - "$OLD_URL" "$NEW_URL" <<'PY'
import datetime, sys, urllib.parse
def ex(u):
    q = urllib.parse.parse_qs(urllib.parse.urlparse(u).query)
    return datetime.datetime.fromtimestamp(int(q["ex"][0], 16), datetime.UTC)
old, new = ex(sys.argv[1]), ex(sys.argv[2])
print(f"   เดิม ex = {old}")
print(f"   ใหม่ ex = {new}")
print("   -> ได้ลิงก์ใหม่" if new > old else "   -> ex ไม่ขยับ: refresh ช่วยอะไรไม่ได้")
PY

echo "3. ดาวน์โหลดด้วย URL ใหม่"
NEW_CODE=$(curl -s -o "$TMP/new" -w '%{http_code}' "$NEW_URL")
GOT=$(sha256sum < "$TMP/new" | cut -d' ' -f1)
echo "   -> HTTP $NEW_CODE, sha256 $([ "$GOT" = "$WANT_SHA" ] && echo MATCH || echo MISMATCH)"

if [ "$NEW_CODE" = 200 ] && [ "$GOT" = "$WANT_SHA" ]; then
  echo
  echo "สรุป: refresh หลังหมดอายุใช้ได้จริง — สมมติฐานของ read path ถูกต้อง"
else
  echo
  echo "สรุป: refresh ไม่ได้ผล — read path ของไฟล์เก่าจะพัง ต้องออกแบบใหม่"
fi
echo
echo "ลบ probe ทิ้ง: curl -X DELETE \"\$HOOK/messages/$MSG\"  แล้ว rm .env.expiry-probe.json"
