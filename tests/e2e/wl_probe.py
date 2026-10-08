#!/usr/bin/env python3
"""A fullscreen GTK window that logs input as JSON lines (input tests in the
virtual display): python3 wl_probe.py LOG
QR_PROBE_IM=1: keys go through an input method first, and its commits are
logged too (text entered by code point, Ctrl+Shift+U)."""
import json
import os
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


im = None
if os.environ.get("QR_PROBE_IM"):
    im = Gtk.IMContextSimple()
    im.connect("commit", lambda _im, text: log(ev="commit", text=text))


def key(_w, ev):
    if im is not None and im.filter_keypress(ev):
        return True
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
if im is not None:
    w.add_events(Gdk.EventMask.KEY_RELEASE_MASK)
    w.connect("key-release-event", lambda _w, ev: im.filter_keypress(ev))
    w.connect("realize", lambda _w: im.set_client_window(w.get_window()))
    w.connect("focus-in-event", lambda *_: im.focus_in() or False)
w.connect("button-press-event", button)
w.connect("motion-notify-event", motion)
w.connect("scroll-event", scroll)
w.connect("destroy", Gtk.main_quit)
w.connect("map-event", lambda *_: log(ev="mapped"))
w.connect("configure-event", lambda _w, e: log(ev="configure", w=e.width, h=e.height) and False)
w.fullscreen()
w.show_all()
Gtk.main()
