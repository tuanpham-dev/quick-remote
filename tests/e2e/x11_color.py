#!/usr/bin/env python3
"""An undecorated window filled with one color at a given place (X11 tests):
python3 x11_color.py COLOR X Y W H"""
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, Gtk  # noqa: E402

color, x, y, w, h = sys.argv[1], *map(int, sys.argv[2:6])
win = Gtk.Window(title=f"qr-color-{color}")
win.set_decorated(False)
win.set_default_size(w, h)
win.move(x, y)
rgba = Gdk.RGBA()
rgba.parse(color)
win.override_background_color(Gtk.StateFlags.NORMAL, rgba)
win.connect("destroy", Gtk.main_quit)
win.show_all()
win.move(x, y)
Gtk.main()
