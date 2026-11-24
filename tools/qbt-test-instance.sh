#!/bin/bash
# A throw-away qBittorrent for integration tests: its own profile directory and Web UI port,
# headless (Qt offscreen), never touching the user's torrents or settings.
#   tools/qbt-test-instance.sh start|stop
# qBittorrent 5.2 only starts the Web UI when a user name and password are set; with
# "bypass authentication for localhost" the API then needs no login.
set -euo pipefail
DIR="${QBT_TEST_DIR:-${TMPDIR:-/tmp}/nezuyomi-qbt-test}"
PORT="${QBT_TEST_PORT:-18080}"
case "${1:-}" in
start)
    rm -rf "$DIR"; mkdir -p "$DIR/qBittorrent/config" "$DIR/dl"
    HASH=$(python3 -I -c "
import hashlib, os, base64
salt = os.urandom(16)
h = hashlib.pbkdf2_hmac('sha512', os.urandom(16).hex().encode(), salt, 100000, 64)
print('@ByteArray(%s:%s)' % (base64.b64encode(salt).decode(), base64.b64encode(h).decode()))")
    cat > "$DIR/qBittorrent/config/qBittorrent.conf" <<CONF
[Meta]
MigrationVersion=8

[LegalNotice]
Accepted=true

[BitTorrent]
Session\\DefaultSavePath=$DIR/dl
Session\\Port=$((PORT + 1000))

[Preferences]
WebUI\\Username=test
WebUI\\Password_PBKDF2=$HASH
WebUI\\Enabled=true
WebUI\\Address=127.0.0.1
WebUI\\Port=$PORT
WebUI\\LocalHostAuth=false
CONF
    QT_QPA_PLATFORM=offscreen nohup qbittorrent --profile="$DIR" > "$DIR/stdout.log" 2>&1 &
    echo $! > "$DIR/pid"
    for _ in $(seq 40); do curl -sf "http://127.0.0.1:$PORT/api/v2/app/version" >/dev/null 2>&1 && break; sleep 0.5; done
    echo "QBT_TEST_PORT=$PORT QBT_TEST_DIR=$DIR"
    ;;
stop)
    [ -f "$DIR/pid" ] && kill "$(cat "$DIR/pid")" 2>/dev/null || true
    ;;
*) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
