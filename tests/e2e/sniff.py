#!/usr/bin/env python3
"""Captures every packet on all interfaces of this network namespace for
N seconds and appends the raw bytes to OUT (needs CAP_NET_RAW).

usage: sniff.py SECONDS OUT"""
import socket, sys, time
secs, out = float(sys.argv[1]), sys.argv[2]
s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.ntohs(3))
s.settimeout(0.2)
end = time.time() + secs
n = 0
with open(out, "wb") as f:
    while time.time() < end:
        try:
            pkt = s.recv(65535)
        except socket.timeout:
            continue
        f.write(pkt)
        n += 1
print(n)
