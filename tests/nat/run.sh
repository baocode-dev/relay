#!/usr/bin/env bash
# Two devices behind two NATs, a relay on the "internet" between them, all
# in Linux network namespaces on one machine. Run as root (CI: sudo).
#
#   tests/nat/run.sh [target-dir]     builds if no binaries are given
#
# Cases:
#   easy     both NATs map a socket to one outside port (cone): expect a
#            direct path, punched through.
#   hard     one NAT picks a new random port for every destination
#            (symmetric): a connection must work, direct or relayed.
#   no-udp   one side's network drops all UDP: expect the relay, still
#            working (WebSocket over TLS on 443).
set -euo pipefail

cd "$(dirname "$0")/../.."
TARGET=${1:-target/release}
if [[ ! -x $TARGET/baocode-relay || ! -x $TARGET/examples/peer ]]; then
  cargo build --release --bin baocode-relay --example peer
  TARGET=target/release
fi
RELAY=$(realpath "$TARGET/baocode-relay")
PEER=$(realpath "$TARGET/examples/peer")
WORK=$(mktemp -d)
NS=(nat-wan nat-r1 nat-r2 nat-a nat-b)

clean_ns() {
  for ns in "${NS[@]}"; do
    ip netns pids "$ns" 2>/dev/null | xargs -r kill 2>/dev/null || true
    ip netns del "$ns" 2>/dev/null || true
  done
}
trap 'clean_ns; rm -rf "$WORK"' EXIT
clean_ns

on() { local ns=$1; shift; ip netns exec "$ns" "$@"; }

for ns in "${NS[@]}"; do
  ip netns add "$ns"
  on "$ns" ip link set lo up
done

# The internet: a bridge in nat-wan, 10.0.0.0/24. The relay is 10.0.0.1.
on nat-wan ip link add br0 type bridge
on nat-wan ip addr add 10.0.0.1/24 dev br0
on nat-wan ip link set br0 up

# router <n>: 10.0.0.1<n> outside, 192.168.<n>.1 inside, host 192.168.<n>.2.
router() {
  local n=$1 r=nat-r$1 host=$2
  ip link add "w$n" netns nat-wan type veth peer name wan netns "$r"
  on nat-wan ip link set "w$n" master br0 up
  on "$r" ip addr add "10.0.0.1$n/24" dev wan
  on "$r" ip link set wan up
  ip link add lan netns "$r" type veth peer name eth0 netns "$host"
  on "$r" ip addr add "192.168.$n.1/24" dev lan
  on "$r" ip link set lan up
  on "$host" ip addr add "192.168.$n.2/24" dev eth0
  on "$host" ip link set eth0 up
  on "$host" ip route add default via "192.168.$n.1"
  on "$r" sysctl -qw net.ipv4.ip_forward=1
}
router 1 nat-a
router 2 nat-b

nat() {
  local r=nat-r$1 mode=$2
  on "$r" iptables -t nat -F
  on "$r" iptables -F
  # Like a real router: nothing unasked-for reaches the router itself.
  # (Without it, a punch arriving before ours goes out leaves a conntrack
  # entry that makes MASQUERADE pick another port: symmetric by accident.)
  on "$r" iptables -A INPUT -i wan -m conntrack --ctstate NEW -j DROP
  case $mode in
    cone) on "$r" iptables -t nat -A POSTROUTING -o wan -j MASQUERADE ;;
    symmetric) on "$r" iptables -t nat -A POSTROUTING -o wan -j MASQUERADE --random-fully ;;
    no-udp)
      on "$r" iptables -t nat -A POSTROUTING -o wan -j MASQUERADE
      on "$r" iptables -A FORWARD -p udp -j DROP
      ;;
  esac
  # Forget mappings from the case before.
  on "$r" conntrack -F 2>/dev/null || true
}

# A certificate for 10.0.0.1, trusted by the peers.
"$PEER" cert 10.0.0.1 "$WORK"
cat > "$WORK/relay.toml" <<EOF
hostname = "10.0.0.1"
metrics = false
[bind]
http = "10.0.0.1:80"
https = "10.0.0.1:443"
quic = "10.0.0.1:7842"
[tls]
mode = "files"
cert = "$WORK/cert.pem"
key = "$WORK/key.pem"
EOF
on nat-wan "$RELAY" --config "$WORK/relay.toml" 2> "$WORK/relay.log" &
sleep 1

URL=https://10.0.0.1
failed=0

case_() {
  local name=$1 r1=$2 r2=$3 expect=$4
  nat 1 "$r1"
  nat 2 "$r2"
  on nat-a "$PEER" listen "$URL" "$WORK/cert.pem" > "$WORK/$name.id" 2> "$WORK/$name.listen.log" &
  local listener=$!
  for _ in $(seq 100); do [[ -s $WORK/$name.id ]] && break; sleep 0.2; done
  local id
  id=$(head -1 "$WORK/$name.id")
  if [[ -z $id ]]; then
    echo "[$name] FAIL: the listener never came up"; cat "$WORK/$name.listen.log"; failed=1
    kill $listener 2>/dev/null || true
    return
  fi
  local out
  if ! out=$(on nat-b timeout 90 "$PEER" dial "$URL" "$id" "$WORK/cert.pem" 2> "$WORK/$name.dial.log"); then
    echo "[$name] FAIL: no connection"; echo "$out"; tail -20 "$WORK/$name.dial.log"; failed=1
  else
    local path
    path=$(grep '^path:' <<< "$out")
    echo "[$name] $path; $(grep '^echo:' <<< "$out")"
    case $expect in
      direct) [[ $path == "path: direct"* ]] || { echo "[$name] FAIL: expected direct"; failed=1; } ;;
      relay) [[ $path == "path: relay" ]] || { echo "[$name] FAIL: expected relay"; failed=1; } ;;
      any) ;;
    esac
  fi
  kill $listener 2>/dev/null || true
  wait $listener 2>/dev/null || true
}

case_ easy cone cone direct
case_ hard cone symmetric any
case_ no-udp cone no-udp relay

if [[ $failed != 0 ]]; then
  echo "--- relay log"; tail -50 "$WORK/relay.log"
  exit 1
fi
echo "all NAT cases passed"
