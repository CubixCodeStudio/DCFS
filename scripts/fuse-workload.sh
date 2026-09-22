#!/usr/bin/env bash
# Real-usage exercise: drive the mount with the tools people actually use.
#
# Linux only: needs /dev/fuse and fuse3, plus git, rsync, sqlite3 and tar.
# Starts its own server, so set DATABASE_URL to a THROWAWAY database.
#
#   DATABASE_URL=postgresql://... MODE=mirror scripts/fuse-workload.sh
#
# Unlike fuse-smoke.sh this reports every case and never aborts early, so one
# regression does not hide the rest.
set -uo pipefail
export MASTER_KEY=$(openssl rand -hex 32) API_TOKEN=$(openssl rand -hex 32)
export SERVER_ADDR=127.0.0.1:8080 RUST_LOG=warn
export OBJECT_STORE_PATH=$(mktemp -d) GC_RETENTION_SECS=3600
# PROFILE=release for anything where throughput matters: debug builds run the
# encryption and hashing an order of magnitude slower.
PROFILE="${PROFILE:-debug}"
[ "$PROFILE" = release ] && flag=--release || flag=
cargo build -q $flag --bin discordfs-server --bin discordfs-fuse || exit 1
bin="${CARGO_TARGET_DIR:-target}/$PROFILE"

mnt=$(mktemp -d); work=$(mktemp -d)
"$bin/discordfs-server" > /tmp/server.log 2>&1 &
for _ in $(seq 1 60); do curl -sf http://$SERVER_ADDR/health >/dev/null && break; sleep 1; done
DISCORDFS_TOKEN=$API_TOKEN "$bin/discordfs-fuse" "$mnt" --server http://$SERVER_ADDR --mode "${MODE:-stream}" > /tmp/fuse.log 2>&1 &
for _ in $(seq 1 30); do mountpoint -q "$mnt" && break; sleep 1; done
mountpoint -q "$mnt" || { echo "not mounted"; cat /tmp/fuse.log; exit 1; }

pass=0; fail=0
check() { if [ "$1" = 0 ]; then echo "  PASS  $2"; pass=$((pass+1)); else echo "  FAIL  $2"; fail=$((fail+1)); fi; }
t0() { date +%s.%N; }
el() { echo "$1 $2" | awk '{printf "%.1fs", $2-$1}'; }

echo "== 1. shell and coreutils =="
mkdir -p "$mnt/a/b/c" 2>/dev/null; [ -d "$mnt/a/b/c" ]; check $? "mkdir -p nested"
echo hello > "$mnt/a/f.txt"; [ "$(cat "$mnt/a/f.txt")" = hello ]; check $? "write and read"
echo world >> "$mnt/a/f.txt"; [ "$(cat "$mnt/a/f.txt")" = "$(printf 'hello\nworld')" ]; check $? "append with >>"
printf 'x' | tee "$mnt/a/t.txt" >/dev/null; [ "$(cat "$mnt/a/t.txt")" = x ]; check $? "tee"
cp -r "$mnt/a" "$mnt/a-copy" 2>/dev/null && diff -r "$mnt/a" "$mnt/a-copy" >/dev/null; check $? "cp -r a directory tree"
touch "$mnt/a/empty"; [ -f "$mnt/a/empty" ] && [ ! -s "$mnt/a/empty" ]; check $? "touch creates an empty file"
chmod 600 "$mnt/a/f.txt" && [ "$(stat -c %a "$mnt/a/f.txt")" = 600 ]; check $? "chmod"
find "$mnt" -type f >/dev/null 2>&1; check $? "find"
du -sh "$mnt" >/dev/null 2>&1; check $? "du"
grep -r hello "$mnt/a" >/dev/null 2>&1; check $? "grep -r"
ln -s f.txt "$mnt/a/link" 2>/dev/null; check $? "symlink create"
[ "$(readlink "$mnt/a/link")" = f.txt ]; check $? "readlink"
[ "$(cat "$mnt/a/link")" = "$(cat "$mnt/a/f.txt")" ]; check $? "reading through a symlink"
ln -s /nowhere "$mnt/a/dangling" 2>/dev/null && [ -L "$mnt/a/dangling" ] && [ ! -e "$mnt/a/dangling" ]; check $? "dangling symlink"
[ "$(ls -l "$mnt/a/link" | cut -c1)" = l ]; check $? "ls shows it as a link"
rm "$mnt/a/dangling" && [ ! -L "$mnt/a/dangling" ]; check $? "rm a symlink"

echo "== 2. odd filenames =="
touch "$mnt/with space.txt" && [ -f "$mnt/with space.txt" ]; check $? "name with a space"
touch "$mnt/ชื่อไทย.txt" && [ -f "$mnt/ชื่อไทย.txt" ]; check $? "UTF-8 name"
touch "$mnt/emoji-📁.txt" && [ -f "$mnt/emoji-📁.txt" ]; check $? "emoji in the name"
name=$(printf 'raw-\xff\xfe')
touch "$mnt/$name" 2>/dev/null && [ -e "$mnt/$name" ]; check $? "non-UTF-8 byte name"
ls "$mnt" >/dev/null 2>&1; check $? "ls with those names present"

echo "== 3. large file =="
head -c 52428800 /dev/urandom > "$work/50m.bin"
s=$(t0); cp "$work/50m.bin" "$mnt/50m.bin"; e=$(t0)
[ "$(sha256sum < "$work/50m.bin" | cut -d' ' -f1)" = "$(sha256sum < "$mnt/50m.bin" | cut -d' ' -f1)" ]
check $? "50 MB write + checksum ($(el $s $e))"
s=$(t0); sha256sum < "$mnt/50m.bin" >/dev/null; e=$(t0); check $? "50 MB sequential read ($(el $s $e))"

# Copying the same file several times in a row: fast back-to-back copies once
# read back truncated, because an attribute reply that lagged the buffered
# writes shrank the kernel's idea of the file.
want=$(sha256sum < "$work/50m.bin" | cut -d' ' -f1)
repeat_ok=0
for a in 1 2 3; do
    cp "$work/50m.bin" "$mnt/repeat-$a.bin"
    [ "$(sha256sum < "$mnt/repeat-$a.bin" | cut -d' ' -f1)" = "$want" ] || repeat_ok=1
done
check $repeat_ok "three back-to-back 50 MB copies all read back whole"
s=$(t0)
for off in 0 12345678 33333333 49999999; do dd if="$mnt/50m.bin" bs=1 skip=$off count=64 status=none | cmp -s - <(dd if="$work/50m.bin" bs=1 skip=$off count=64 status=none) || exit 1; done
e=$(t0); check $? "4 random 64-byte reads from a 50 MB file ($(el $s $e))"
dd if=/dev/zero of="$mnt/50m.bin" bs=1M seek=20 count=1 conv=notrunc status=none
dd if=/dev/zero of="$work/50m.bin" bs=1M seek=20 count=1 conv=notrunc status=none
[ "$(sha256sum < "$work/50m.bin" | cut -d' ' -f1)" = "$(sha256sum < "$mnt/50m.bin" | cut -d' ' -f1)" ]
check $? "1 MB overwrite in the middle of a 50 MB file"

echo "== 4. archives and sync =="
mkdir -p "$work/withlinks/sub" && echo data > "$work/withlinks/real.txt" && ln -s real.txt "$work/withlinks/alias" && ln -s ../real.txt "$work/withlinks/sub/up"
tar -cf "$mnt/links.tar" -C "$work" withlinks 2>/dev/null && mkdir -p "$mnt/links-out" && tar -xf "$mnt/links.tar" -C "$mnt/links-out" 2>/dev/null
[ -L "$mnt/links-out/withlinks/alias" ] && [ "$(readlink "$mnt/links-out/withlinks/sub/up")" = ../real.txt ]
check $? "tar round trip preserving symlinks"
rsync -a "$work/withlinks/" "$mnt/rsync-links/" >/dev/null 2>&1 && [ -L "$mnt/rsync-links/alias" ]; check $? "rsync -a preserves symlinks"
tar -cf "$mnt/src.tar" -C "${REPO:-/src}" crates 2>/dev/null; check $? "tar create onto the mount"
mkdir -p "$mnt/untar" && tar -xf "$mnt/src.tar" -C "$mnt/untar" 2>/dev/null; check $? "tar extract onto the mount"
diff -r /src/crates "$mnt/untar/crates" >/dev/null 2>&1; check $? "extracted tree matches the original"
s=$(t0); rsync -a "${REPO:-/src}"/crates/discordfs-core/ "$mnt/rsync-dest/" >/dev/null 2>&1; e=$(t0)
diff -r "${REPO:-/src}"/crates/discordfs-core "$mnt/rsync-dest" >/dev/null 2>&1; check $? "rsync a source tree ($(el $s $e))"

echo "== 5. git =="
git config --global user.email t@example.com >/dev/null 2>&1
git config --global user.name Test >/dev/null 2>&1
git config --global init.defaultBranch main >/dev/null 2>&1
(cd "$mnt" && git init -q repo) 2>/dev/null; check $? "git init on the mount"
(cd "$mnt/repo" && cp -r "${REPO:-/src}"/crates/discordfs-core . && git add -A && git commit -qm first) >/dev/null 2>&1
check $? "git add + commit"
(cd "$mnt/repo" && git status --porcelain | head -1 >/dev/null && git log --oneline | head -1 >/dev/null) 2>/dev/null
check $? "git status + git log"
(cd "$mnt/repo" && echo change >> discordfs-core/src/lib.rs && git add -A && git commit -qm second && git diff HEAD~1 --stat >/dev/null) 2>/dev/null
check $? "git second commit + diff"
(cd "$mnt/repo" && git fsck --no-progress >/dev/null 2>&1); check $? "git fsck"

echo "== 6. sqlite (random access + fsync) =="
if command -v sqlite3 >/dev/null; then
  sqlite3 "$mnt/test.db" "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);" 2>/dev/null; check $? "sqlite create"
  sqlite3 "$mnt/test.db" "BEGIN; $(for i in $(seq 1 500); do echo "INSERT INTO t(v) VALUES('row$i');"; done) COMMIT;" 2>/dev/null; check $? "sqlite 500 inserts"
  [ "$(sqlite3 "$mnt/test.db" 'SELECT count(*) FROM t;' 2>/dev/null)" = 500 ]; check $? "sqlite count"
  sqlite3 "$mnt/test.db" "PRAGMA integrity_check;" 2>/dev/null | grep -q ok; check $? "sqlite integrity_check"
else
  echo "  SKIP  sqlite3 not installed"
fi

echo "== 7. concurrency =="
pids=""; for i in 1 2 3 4 5 6 7 8; do ( head -c 2000000 /dev/urandom > "$mnt/par-$i.bin" ) & pids="$pids $!"; done
for p in $pids; do wait "$p"; done
ok=0; for i in 1 2 3 4 5 6 7 8; do [ "$(stat -c %s "$mnt/par-$i.bin")" = 2000000 ] || ok=1; done
check $ok "8 concurrent writers to different files"
printf 'seed' > "$mnt/shared.txt"
pids=""; for i in $(seq 1 8); do ( cat "$mnt/shared.txt" >/dev/null ) & pids="$pids $!"; done
rc=0; for p in $pids; do wait "$p" || rc=1; done; check $rc "8 concurrent readers of one file"

echo "== 8. durability across a remount =="
before=$(find "$mnt" -type f | wc -l)
sum=$(sha256sum < "$mnt/50m.bin" | cut -d' ' -f1)
fusermount3 -u "$mnt"
DISCORDFS_TOKEN=$API_TOKEN "$bin/discordfs-fuse" "$mnt" --server http://$SERVER_ADDR --mode "${MODE:-stream}" >> /tmp/fuse.log 2>&1 &
for _ in $(seq 1 30); do mountpoint -q "$mnt" && break; sleep 1; done
[ "$(find "$mnt" -type f | wc -l)" = "$before" ]; check $? "file count survives a remount ($before files)"
[ "$(sha256sum < "$mnt/50m.bin" | cut -d' ' -f1)" = "$sum" ]; check $? "50 MB checksum survives a remount"
(cd "$mnt/repo" && git fsck --no-progress >/dev/null 2>&1); check $? "git repo still valid after a remount"

echo "== 9. deletion =="
rm -rf "$mnt/untar" "$mnt/rsync-dest" 2>/dev/null; [ ! -d "$mnt/untar" ]; check $? "rm -rf a directory tree"
rm "$mnt/50m.bin"; [ ! -f "$mnt/50m.bin" ]; check $? "rm a large file"

echo
echo "RESULT: $pass passed, $fail failed"
echo "== server warnings =="; grep -iE "error|warn" /tmp/server.log | sort | uniq -c | sort -rn | head -5
echo "== client warnings =="; grep -iE "error|warn" /tmp/fuse.log | sort | uniq -c | sort -rn | head -5
fusermount3 -u "$mnt" 2>/dev/null

[ "$fail" -eq 0 ]
