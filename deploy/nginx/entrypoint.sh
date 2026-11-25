#!/bin/sh
# Entry point of the nginx container (started with "sh", so the file needs no exec bit).
#
# Starts nginx in the foreground, then every WATCH_INTERVAL seconds:
#   * if the backend map written by the Router changed, or
#   * if the Router asked for a reload (reload.request),
# runs "nginx -t" and, when the test passes, "nginx -s reload". A failing test keeps the running
# configuration. The outcome is written to nginx-status.json, which the admin page shows.
#
# Before starting nginx it also writes two generated files into $GEN_DIR (included by nginx.conf):
#   * cors_origins.map : one entry per origin of CORS_ALLOWED_ORIGINS (comma or space separated), the browser
#                        origins of the frontend allowed to call the Router and the backends behind it;
#   * ca-bundle.crt    : the system CA bundle plus every deploy/certs/*.crt mounted at $EXTRA_CA_DIR, used to
#                        verify backend certificates on the internal sync listener.
#
# This avoids giving the Router container access to the Docker socket.

set -u

STATE_DIR="${STATE_DIR:-/etc/nginx/router}"
INTERVAL="${WATCH_INTERVAL:-3}"
STATUS="$STATE_DIR/nginx-status.json"
REQUEST="$STATE_DIR/reload.request"

GEN_DIR="${GEN_DIR:-/etc/nginx/generated}"
EXTRA_CA_DIR="${EXTRA_CA_DIR:-/etc/nginx/ca}"

log() { echo "[nginx-entrypoint] $*" >&2; }

write_cors_map() {
    out="$GEN_DIR/cors_origins.map"
    : > "$out.tmp"
    for origin in $(printf '%s' "${CORS_ALLOWED_ORIGINS:-http://localhost:3000}" | tr ',' ' '); do
        origin="${origin%/}"
        case "$origin" in
            http://*|https://*) ;;
            *) log "CORS_ALLOWED_ORIGINS: ignoring \"$origin\" (must start with http:// or https://)"; continue ;;
        esac
        if printf '%s' "$origin" | grep -Eq '^https?://[][A-Za-z0-9.:-]+$'; then
            printf '"%s" $http_origin;\n' "$origin" >> "$out.tmp"
        else
            log "CORS_ALLOWED_ORIGINS: ignoring \"$origin\" (scheme://host[:port] only)"
        fi
    done
    mv -f "$out.tmp" "$out"
    log "CORS origins: $(tr '\n' ' ' < "$out")"
}

write_ca_bundle() {
    out="$GEN_DIR/ca-bundle.crt"
    cat /etc/ssl/certs/ca-certificates.crt > "$out.tmp" 2>/dev/null || : > "$out.tmp"
    for f in "$EXTRA_CA_DIR"/*.crt; do
        [ -f "$f" ] && { cat "$f"; echo; } >> "$out.tmp"
    done
    mv -f "$out.tmp" "$out"
}

# Checksum of every map (names + content), so add/remove/edit are all detected.
checksum() {
    for f in "$STATE_DIR"/*.map; do
        [ -f "$f" ] && { echo "$f"; cat "$f"; }
    done | md5sum | cut -d' ' -f1
}

# JSON string escape: backslash, quote, newlines.
json_escape() {
    sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/ /g' | awk 'BEGIN { ORS = "\\n" } { print }'
}

write_status() {
    ok="$1"; action="$2"; message="$3"
    now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    msg="$(printf '%s' "$message" | json_escape)"
    printf '{"ok": %s, "action": "%s", "time": "%s", "message": "%s"}\n' "$ok" "$action" "$now" "$msg" \
        > "$STATUS.tmp" && mv -f "$STATUS.tmp" "$STATUS"
}

test_and_reload() {
    action="$1"
    if out="$(nginx -t 2>&1)"; then
        if rout="$(nginx -s reload 2>&1)"; then
            log "$action: reloaded"
            write_status true "$action: reloaded" "$out"
        else
            log "$action: reload failed: $rout"
            write_status false "$action: reload failed" "$rout"
        fi
    else
        log "$action: nginx -t failed, keeping the running configuration"
        log "$out"
        write_status false "$action: nginx -t failed, keeping the running configuration" "$out"
    fi
}

mkdir -p "$STATE_DIR" "$GEN_DIR"
write_cors_map
write_ca_bundle
# The Router runs as uid 65532 and writes the generated map + reload requests here.
chown "${ROUTER_UID:-65532}:${ROUTER_UID:-65532}" "$STATE_DIR" 2>/dev/null || true

nginx -g 'daemon off;' &
NGINX_PID=$!
trap 'log "stopping"; nginx -s quit 2>/dev/null; wait "$NGINX_PID"; exit 0' TERM INT QUIT

sleep 1
if kill -0 "$NGINX_PID" 2>/dev/null; then
    write_status true "started" "$(nginx -t 2>&1)"
else
    write_status false "start failed" "$(nginx -t 2>&1)"
fi
last="$(checksum)"
log "watching $STATE_DIR every ${INTERVAL}s"

while kill -0 "$NGINX_PID" 2>/dev/null; do
    sleep "$INTERVAL" &
    wait $!
    current="$(checksum)"
    if [ -f "$REQUEST" ]; then
        rm -f "$REQUEST"
        test_and_reload "reload requested"
        last="$current"
    elif [ "$current" != "$last" ]; then
        test_and_reload "map changed"
        last="$current"
    fi
done

wait "$NGINX_PID"
exit $?
