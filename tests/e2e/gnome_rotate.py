#!/usr/bin/env python3
"""Rotates the GNOME (mutter) session's first monitor, for tests:
  gnome_rotate.py left|none
through DisplayConfig.ApplyMonitorsConfig (temporary, no confirmation)."""
import sys

from gi.repository import Gio, GLib

DC = "org.gnome.Mutter.DisplayConfig"
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
serial, monitors, logical, _props = bus.call_sync(
    DC, "/org/gnome/Mutter/DisplayConfig", DC, "GetCurrentState", None, None, 0, -1, None).unpack()
transform = {"none": 0, "left": 1}[sys.argv[1]]
configs = []
for x, y, scale, _t, primary, specs, _p in logical:
    mons = []
    for spec in specs:
        _, modes, _ = next(m for m in monitors if m[0] == spec)
        mode = next(m for m in modes if m[6].get("is-current"))
        mons.append((spec[0], mode[0], {}))
    configs.append((x, y, scale, transform, primary, mons))
# Method 1: temporary.
bus.call_sync(DC, "/org/gnome/Mutter/DisplayConfig", DC, "ApplyMonitorsConfig",
              GLib.Variant("(uua(iiduba(ssa{sv}))a{sv})", (serial, 1, configs, {})), None, 0, -1, None)
