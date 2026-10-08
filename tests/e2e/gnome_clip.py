#!/usr/bin/env python3
"""GNOME clipboard for tests, through its own remote-desktop session (the
selection API mutter gives such sessions):
  gnome_clip.py set-text TEXT          (owns the selection for 8 s)
  gnome_clip.py get OUT [SECS]         (the next change by someone else)"""
import os
import sys
import time

from gi.repository import Gio, GLib

RD = "org.gnome.Mutter.RemoteDesktop"
S = RD + ".Session"
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
path = bus.call_sync(RD, "/org/gnome/Mutter/RemoteDesktop", RD, "CreateSession", None,
                     GLib.VariantType("(o)"), 0, -1, None).unpack()[0]


def call(method, args=None, reply="()"):
    return bus.call_sync(RD, path, S, method, args, GLib.VariantType(reply), 0, -1, None)


def call_fd(method, args):
    res, fds = bus.call_with_unix_fd_list_sync(RD, path, S, method, args, GLib.VariantType("(h)"), 0, -1, None, None)
    return fds.get(res.unpack()[0])


call("Start")
call("EnableClipboard", GLib.Variant("(a{sv})", ({},)))
loop = GLib.MainLoop()
start = time.monotonic()
TEXT = "text/plain;charset=utf-8"

if sys.argv[1] == "set-text":
    data = sys.argv[2].encode()
    def transfer(_c, _s, _p, _i, _sig, params):
        mime, serial = params.unpack()
        fd = call_fd("SelectionWrite", GLib.Variant("(u)", (serial,)))
        os.set_blocking(fd, True)
        os.write(fd, data)
        os.close(fd)
        call("SelectionWriteDone", GLib.Variant("(ub)", (serial, True)))

    # Listen before owning the selection: a reader may ask right away.
    bus.signal_subscribe(RD, S, "SelectionTransfer", path, None, 0, transfer)
    call("SetSelection", GLib.Variant("(a{sv})", ({"mime-types": GLib.Variant("as", [TEXT])},)))
    GLib.timeout_add(8000, loop.quit)
    loop.run()
else:
    out, secs = sys.argv[2], float(sys.argv[3]) if len(sys.argv) > 3 else 8

    def changed(_c, _s, _p, _i, _sig, params):
        opts = params.unpack()[0]
        if opts.get("session-is-owner") or time.monotonic() - start < 0.3:
            return
        mimes = opts.get("mime-types", [])
        # mutter sends the list wrapped in a one-field structure.
        if isinstance(mimes, tuple):
            mimes = mimes[0] if mimes else []
        if TEXT not in mimes:
            return
        fd = call_fd("SelectionRead", GLib.Variant("(s)", (TEXT,)))
        os.set_blocking(fd, True)  # mutter hands out a non-blocking pipe
        chunks = []
        while True:
            b = os.read(fd, 65536)
            if not b:
                break
            chunks.append(b)
        os.close(fd)
        open(out, "wb").write(b"".join(chunks))
        loop.quit()

    bus.signal_subscribe(RD, S, "SelectionOwnerChanged", path, None, 0, changed)
    GLib.timeout_add(int(secs * 1000), loop.quit)
    loop.run()
call("Stop")
