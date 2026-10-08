#!/usr/bin/env python3
"""Prints the GNOME (mutter) session's monitors as JSON, for tests:
[{name, x, y, scale, primary, mode: "WxH@R", modes: [...]}]"""
import json

from gi.repository import Gio

DC = "org.gnome.Mutter.DisplayConfig"
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
_, monitors, logical, _ = bus.call_sync(
    DC, "/org/gnome/Mutter/DisplayConfig", DC, "GetCurrentState", None, None, 0, -1, None).unpack()
out = []
for x, y, scale, _t, primary, specs, _p in logical:
    for spec in specs:
        _, modes, _ = next(m for m in monitors if m[0] == spec)
        cur = next((m for m in modes if m[6].get("is-current")), modes[0])
        out.append(dict(name=spec[0], x=x, y=y, scale=scale, primary=primary,
                        mode=f"{cur[1]}x{cur[2]}", modes=[f"{m[1]}x{m[2]}" for m in modes]))
print(json.dumps(out))
