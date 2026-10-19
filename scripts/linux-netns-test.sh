#!/bin/sh
# Linux end-to-end test. Two fake internet links built from network namespaces:
#
#   ns cli (multilink runs here)          ns inet ("the internet")
#     c1 10.1.0.2/24  <--veth-->  r1 10.1.0.1
#        fd01::2/64                  fd01::1
#     c2 10.2.0.2/24  <--veth-->  r2 10.2.0.1      server 198.51.100.10, fd00:100::10 (lo)
#        fd02::2/64                  fd02::1
#
# The server answers TCP :80 and UDP :9999 with the client address it sees, so
# 10.1.0.2 means the flow left through c1, 10.2.0.2 through c2 (fd01::2 / fd02::2 over IPv6).
#
# Run as root with /dev/net/tun, e.g. from macOS (Colima/Docker):
#   docker run --rm --privileged -v "$PWD":/work -w /work rust:1-bookworm sh scripts/linux-netns-test.sh
set -eu
BIN=${BIN:-target-linux/debug/multilink}
SRV=198.51.100.10
SRV6=fd00:100::10
fails=0
check() { if [ "$2" = "$3" ]; then echo "PASS $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }
cli() { ip netns exec cli "$@"; }

for t in curl python3 ip; do command -v $t >/dev/null || missing=1; done
[ -z "${missing:-}" ] || { apt-get update -qq && apt-get install -y -qq iproute2 curl python3 >/dev/null; }
[ -n "${SKIP_BUILD:-}" ] || CARGO_TARGET_DIR=target-linux cargo build -q   # incremental; never test a stale binary

ip netns add inet
ip netns add cli
ip link add c1 netns cli type veth peer r1 netns inet
ip link add c2 netns cli type veth peer r2 netns inet
ip -n inet addr add 10.1.0.1/24 dev r1
ip -n inet addr add 10.2.0.1/24 dev r2
ip -n inet addr add $SRV/32 dev lo
ip -n cli addr add 10.1.0.2/24 dev c1
ip -n cli addr add 10.2.0.2/24 dev c2
# nodad: usable at once instead of after duplicate address detection.
ip -n inet addr add fd01::1/64 dev r1 nodad
ip -n inet addr add fd02::1/64 dev r2 nodad
ip -n inet addr add $SRV6/128 dev lo
ip -n cli addr add fd01::2/64 dev c1 nodad
ip -n cli addr add fd02::2/64 dev c2 nodad
for l in "inet r1" "inet r2" "inet lo" "cli c1" "cli c2" "cli lo"; do set -- $l; ip -n "$1" link set "$2" up; done
# One default route per link, as NetworkManager sets them up (different metrics).
ip -n cli route add default via 10.1.0.1 dev c1 metric 100
ip -n cli route add default via 10.2.0.1 dev c2 metric 200
ip -n cli -6 route add default via fd01::1 dev c1 metric 100
ip -n cli -6 route add default via fd02::1 dev c2 metric 200

ip netns exec inet python3 - <<'EOF' &
import http.server, socket, threading
def udp():
    # Bound to the service address so replies come from it, as a real server's would.
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("198.51.100.10", 9999))
    while True:
        _, a = s.recvfrom(100); s.sendto(a[0].encode(), a)
threading.Thread(target=udp, daemon=True).start()
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        b = self.client_address[0].encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
class H6(http.server.ThreadingHTTPServer): address_family = socket.AF_INET6
threading.Thread(target=H6(("fd00:100::10", 80), H).serve_forever, daemon=True).start()
http.server.ThreadingHTTPServer(("0.0.0.0", 80), H).serve_forever()
EOF
SERVER=$!
sleep 1

seen() { cli curl -s -m 5 http://$SRV/ || echo none; }
seen6() { cli curl -s -m 5 "http://[$SRV6]/" || echo none; }
seen_udp() { cli python3 -c "import socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(3); s.sendto(b'x',('$SRV',9999)); print(s.recvfrom(100)[0].decode())" 2>/dev/null || echo none; }
# Not via cli(): $! must be multilink itself (ip netns exec execs it), not a subshell.
start() { ip netns exec cli "$BIN" run --probe $SRV:80 --probe6 "${P6:-[$SRV6]:80}" "$@" > /tmp/ml.log 2>&1 & ML=$!; sleep 4; }
stop() { kill -TERM $ML; wait $ML || true; [ -z "${DEBUG:-}" ] || { echo "--- log"; cat /tmp/ml.log; }; }

echo "== round robin"
start --policy round-robin
check "rr alternates" "$(seen) $(seen) $(seen) $(seen)" "10.1.0.2 10.2.0.2 10.1.0.2 10.2.0.2"
check "udp relayed" "$(seen_udp | cut -d. -f1)" "10"
check "capture routes present" "$(cli ip route | grep -c '/1 dev tun')" "2"
check "status page counts bytes live" "$(cli curl -s http://127.0.0.1:7878/api/status | python3 -c 'import json,sys; print(all(l["bytes_down"] > 0 for l in json.load(sys.stdin)["links"]))')" "True"
check "ipv6 capture routes present" "$(cli ip -6 route | grep -c '/1 dev tun')" "2"
check "ipv6 rr uses both links" "$(for i in 1 2 3 4; do seen6; echo; done | sort -u | tr '\n' ' ')" "fd01::2 fd02::2 "
check "status page shows ipv6 up" "$(cli curl -s http://127.0.0.1:7878/api/status | python3 -c 'import json,sys; print(all(l["up6"] for l in json.load(sys.stdin)["links"]))')" "True"
stop
check "rr: /1 routes removed" "$(cli ip route | grep -c '/1 dev' || true)" "0"
check "rr: ipv6 /1 routes removed" "$(cli ip -6 route | grep -c '/1 dev' || true)" "0"
check "rr: tun device gone" "$(cli ip -o link | grep -c tun || true)" "0"

echo "== sticky + failover"
start
a=$(seen); b=$(seen)
check "sticky: same link twice" "$a" "$b"
# Break the link sticky picked by removing its gateway's address. The interface keeps its
# carrier, so only the health probe can notice (macOS testing covered "interface gone").
if [ "$a" = "10.1.0.2" ]; then dead=r1 gw=10.1.0.1/24 other=10.2.0.2; else dead=r2 gw=10.2.0.1/24 other=10.1.0.2; fi
ip -n inet addr del $gw dev $dead
sleep 7
check "failover to other link" "$(seen)" "$other"
check "down event logged" "$(grep -v ' ipv6 ' /tmp/ml.log | grep -c ' down$')" "1"
ip -n inet addr add $gw dev $dead
sleep 4
check "rejoin: sticky back on first link" "$(seen)" "$a"
check "up events: 2 at start + 1 rejoin" "$(grep -v ' ipv6 ' /tmp/ml.log | grep -c ' link .* up (')" "3"
a6=$(seen6)
check "ipv6 sticky: same link twice" "$(seen6)" "$a6"
# Same trick for IPv6 only: IPv4 keeps working on both links.
if [ "$a6" = "fd01::2" ]; then dead=r1 gw=fd01::1/64 other=fd02::2; else dead=r2 gw=fd02::1/64 other=fd01::2; fi
ip -n inet addr del $gw dev $dead
sleep 7
check "ipv6 failover to other link" "$(seen6)" "$other"
check "ipv6 down event logged" "$(grep -c ' ipv6 link .* down$' /tmp/ml.log)" "1"
ip -n inet addr add $gw dev $dead nodad
stop

echo "== ipv6 capture follows ipv6 health"
P6=off start
check "probe6 off: no ipv6 capture" "$(cli ip -6 route | grep -c '/1 dev' || true)" "0"
check "probe6 off: ipv6 still works outside the tunnel" "$(seen6)" "fd01::2"
stop
# Nothing answers here, so no link passes the IPv6 probe.
P6='[fd00:100::99]:80' start
check "no ipv6 link healthy: no ipv6 capture" "$(cli ip -6 route | grep -c '/1 dev' || true)" "0"
stop

echo "== status page is read-only"
start
check "no stop over HTTP" "$(cli curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:7878/api/stop)" "404"
check "still running" "$(kill -0 $ML 2>/dev/null && echo running || echo exited)" "running"
stop

echo "== kill -9 cleanup"
start
kill -9 $ML; wait $ML 2>/dev/null || true; sleep 1
check "kill -9: /1 routes gone" "$(cli ip route | grep -c '/1 dev' || true)" "0"
check "kill -9: ipv6 /1 routes gone" "$(cli ip -6 route | grep -c '/1 dev' || true)" "0"
check "kill -9: tun device gone" "$(cli ip -o link | grep -c tun || true)" "0"
check "defaults untouched" "$(cli ip route | grep -c '^default')" "2"

echo "== log of last run"; cat /tmp/ml.log
kill $SERVER
[ $fails -eq 0 ] && echo "ALL PASSED" || { echo "$fails FAILED"; exit 1; }
