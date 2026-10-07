#!/usr/bin/env python3
"""A fullscreen GTK window that logs input as JSON lines (input tests in the
virtual display): python3 wl_probe.py LOG"""
import json
import sys
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk  # noqa: E402

out = open(sys.argv[1], "a", buffering=1)


def log(**kw):
    kw["t"] = time.monotonic()
    out.write(json.dumps(kw) + "\n")


def key(_w, ev):
    c = Gdk.keyval_to_unicode(ev.keyval)
    log(ev="key", keyval=Gdk.keyval_name(ev.keyval), char=chr(c) if c else "", code=ev.hardware_keycode)
    return True


def button(_w, ev):
    log(ev="button", button=ev.button, x=round(ev.x), y=round(ev.y))
    return True


def motion(_w, ev):
    log(ev="motion", x=round(ev.x), y=round(ev.y))
    return True


def scroll(_w, ev):
    ok, dx, dy = ev.get_scroll_deltas()
    log(ev="scroll", direction=int(ev.direction), dx=dx if ok else 0, dy=dy if ok else 0)
    return True


w = Gtk.Window(title="qr-probe")
w.add_events(
    Gdk.EventMask.KEY_PRESS_MASK
    | Gdk.EventMask.BUTTON_PRESS_MASK
    | Gdk.EventMask.POINTER_MOTION_MASK
    | Gdk.EventMask.SCROLL_MASK
    | Gdk.EventMask.SMOOTH_SCROLL_MASK
)
w.connect("key-press-event", key)
w.connect("button-press-event", button)
w.connect("motion-notify-event", motion)
w.connect("scroll-event", scroll)
w.connect("destroy", Gtk.main_quit)
w.connect("map-event", lambda *_: log(ev="mapped"))
w.fullscreen()
w.show_all()
Gtk.main()
