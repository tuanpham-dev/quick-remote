#!/bin/bash
# Builds a test network in an unprivileged user + network namespace and runs
# a script inside it.
#
#   tests/e2e/netns.sh <topology> <script> [args...]
#
# The script is sourced with these helpers and variables:
#   on <node> <cmd...>   run a command in a node's network namespace
#   bg <node> <name> <cmd...>   same, in the background; PID in $PID_<name>, log in $LOGDIR/<name>.log
#   shape <node> <dev> <netem args...>   replace the netem qdisc on a device
#   $IP_<node>           the node's address (for routed topologies, its LAN address)
#   $LOGDIR              per-run log directory
#
# Topologies:
#   lan        host —— client: 1 Gbit/s, 0.1 ms each way, 10.77.0.1 / .2
#   wan        host —— router —— client: 10 Mbit/s, 50 ms RTT, 1% loss host→client (0.5% per hop)
#   nat-cone   host — natH — inet(relay) — natC — client, port-preserving NAT
#   nat-sym    the same with fully random NAT ports (symmetric)
set -euo pipefail

if [[ "${1:-}" != __inner ]]; then
	[[ $# -ge 2 ]] || { sed -n '2,20p' "$0"; exit 2; }
	mkdir -p "$(dirname "$0")/../../out"
	export LOGDIR="${LOGDIR:-$(mktemp -d "$(dirname "$0")/../../out/e2e.XXXXXX")}"
	exec unshare --user --map-root-user --net --fork --kill-child bash "$0" __inner "$@"
fi
shift
topo="$1"
script="$2"
shift 2

declare -A NODE_PID=()
LINKS=0
cleanup() {
	# Everything started in a node's namespace (hosts, Xvfb, …), not just the
	# shell jobs: nsenter children outlive their job otherwise.
	for p in "${NODE_PID[@]}"; do
		ns=$(readlink "/proc/$p/ns/net" 2>/dev/null) || continue
		for q in /proc/[0-9]*; do
			# A process may exit between the check and the kill: under
			# `set -e` that failed kill made the whole run exit 1.
			[[ "$(readlink "$q/ns/net" 2>/dev/null)" == "$ns" ]] && { kill -9 "${q#/proc/}" 2>/dev/null || true; }
		done
	done
	for p in "${NODE_PID[@]}"; do kill "$p" 2>/dev/null || true; done
	jobs -p | xargs -r kill 2>/dev/null || true
}
trap cleanup EXIT

node() {
	unshare --net sleep infinity &
	NODE_PID[$1]=$!
	# Wait until the child has entered its namespace.
	for _ in $(seq 100); do
		[[ "$(readlink /proc/$!/ns/net)" != "$(readlink /proc/self/ns/net)" ]] && break
		sleep 0.01
	done
	on "$1" ip link set lo up
}

on() {
	local n="$1"
	shift
	nsenter --target "${NODE_PID[$n]}" --net -- "$@"
}

bg() {
	local n="$1" name="$2"
	shift 2
	on "$n" "$@" >"$LOGDIR/$name.log" 2>&1 &
	printf -v "PID_$name" '%s' "$!"
}

# link <nodeA> <devA> <addrA/len> <nodeB> <devB> <addrB/len> <netem args...>
link() {
	local na=$1 da=$2 aa=$3 nb=$4 db=$5 ab=$6
	shift 6
	# Temporary names: both ends may share a final name (eth0 — eth0).
	local ta="qa$((LINKS))" tb="qb$((LINKS))"
	LINKS=$((LINKS + 1))
	ip link add "$ta" type veth peer name "$tb"
	ip link set "$ta" netns "${NODE_PID[$na]}"
	ip link set "$tb" netns "${NODE_PID[$nb]}"
	on "$na" ip link set "$ta" name "$da"
	on "$nb" ip link set "$tb" name "$db"
	on "$na" ip addr add "$aa" dev "$da"
	on "$nb" ip addr add "$ab" dev "$db"
	on "$na" ip link set "$da" up
	on "$nb" ip link set "$db" up
	if [[ $# -gt 0 ]]; then
		on "$na" tc qdisc add dev "$da" root netem "$@"
		on "$nb" tc qdisc add dev "$db" root netem "$@"
	fi
}

shape() {
	local n="$1" dev="$2"
	shift 2
	on "$n" tc qdisc replace dev "$dev" root netem "$@"
}

router() {
	on "$1" sysctl -qw net.ipv4.ip_forward=1
}

nat() {
	local n="$1" out="$2" mode="$3"
	local masq="masquerade"
	[[ "$mode" == sym ]] && masq="masquerade fully-random"
	on "$n" nft -f - <<-EOF
		table ip nat {
			chain post { type nat hook postrouting priority 100; oifname "$out" $masq; }
		}
	EOF
}

case "$topo" in
lan)
	node host
	node client
	link host eth0 10.77.0.1/24 client eth0 10.77.0.2/24 rate 1gbit delay 0.1ms limit 100000
	IP_host=10.77.0.1
	IP_client=10.77.0.2
	;;
wan)
	node host
	node router
	node client
	link host eth0 10.10.1.2/24 router eth1 10.10.1.1/24 rate 10mbit delay 25ms loss 0.5% limit 10000
	link client eth0 10.10.2.2/24 router eth2 10.10.2.1/24 rate 10mbit delay 0.01ms loss 0.5% limit 10000
	router router
	on host ip route add default via 10.10.1.1
	on client ip route add default via 10.10.2.1
	IP_host=10.10.1.2
	IP_client=10.10.2.2
	;;
nat-cone | nat-sym)
	mode=cone
	[[ "$topo" == nat-sym ]] && mode=sym
	node host
	node natH
	node inet
	node natC
	node client
	# Private sides.
	link host eth0 192.168.1.10/24 natH lan0 192.168.1.1/24 rate 1gbit delay 1ms
	link client eth0 192.168.2.10/24 natC lan0 192.168.2.1/24 rate 1gbit delay 1ms
	# Public side: inet routes between the two NATs and hosts the relay.
	link natH wan0 198.51.100.2/24 inet h0 198.51.100.1/24 rate 100mbit delay 10ms
	link natC wan0 203.0.113.2/24 inet c0 203.0.113.1/24 rate 100mbit delay 10ms
	router natH
	router natC
	router inet
	on host ip route add default via 192.168.1.1
	on client ip route add default via 192.168.2.1
	on natH ip route add default via 198.51.100.1
	on natC ip route add default via 203.0.113.1
	nat natH wan0 "$mode"
	nat natC wan0 "$mode"
	IP_host=192.168.1.10
	IP_client=192.168.2.10
	IP_relay=198.51.100.1
	;;
*)
	echo "unknown topology $topo" >&2
	exit 2
	;;
esac

export IP_host IP_client
# shellcheck disable=SC1090
source "$script" "$@"
