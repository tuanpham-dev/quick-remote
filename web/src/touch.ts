// Touch gestures on the remote picture: tap = left click, long press =
// right click, one-finger drag = left-button drag, two-finger tap = right
// click, two-finger drag = scroll (or pan while zoomed in), three-finger
// tap = middle click, four-finger tap = keyboard, pinch = view zoom.

import type { View } from "./view.ts";

export interface TouchActions {
  /** The pointer to this remote point. */
  move(p: [number, number]): void;
  button(button: number, down: boolean): void;
  /** Wheel in 1/120-notch units, positive up/right. */
  wheel(dx: number, dy: number): void;
  toggleKeyboard(): void;
}

const LONG_PRESS_MS = 500;
const SLOP_PX = 10;
const TAP_MS = 300;

interface Finger { x: number; y: number; x0: number; y0: number }

export function attachTouch(target: HTMLElement, view: View, act: TouchActions): void {
  const fingers = new Map<number, Finger>();
  let started = 0;
  let maxFingers = 0;
  let moved = false;
  let dragging = false;
  let longTimer: number | undefined;
  let longFired = false;
  let pinch: { dist: number; mx: number; my: number } | null = null;
  let wheelAcc = { x: 0, y: 0 };

  const remote = (f: Finger) => view.toRemote(f.x, f.y);
  const mid = () => {
    const a = [...fingers.values()];
    return { x: a.reduce((s, f) => s + f.x, 0) / a.length, y: a.reduce((s, f) => s + f.y, 0) / a.length };
  };
  const spread = () => {
    const [a, b] = [...fingers.values()];
    return Math.hypot(a.x - b.x, a.y - b.y);
  };
  const cancelLong = () => { if (longTimer !== undefined) { clearTimeout(longTimer); longTimer = undefined; } };

  target.addEventListener("pointerdown", (e) => {
    if (e.pointerType !== "touch") return;
    e.preventDefault();
    target.setPointerCapture?.(e.pointerId);
    if (fingers.size === 0) {
      started = performance.now();
      maxFingers = 0;
      moved = false;
      longFired = false;
      dragging = false;
      pinch = null;
    }
    fingers.set(e.pointerId, { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY });
    maxFingers = Math.max(maxFingers, fingers.size);
    cancelLong();
    if (fingers.size === 1) {
      const p = remote(fingers.get(e.pointerId)!);
      if (p) act.move(p);
      longTimer = window.setTimeout(() => {
        longTimer = undefined;
        if (moved || fingers.size !== 1) return;
        longFired = true;
        act.button(3, true);
        act.button(3, false);
      }, LONG_PRESS_MS);
    } else if (fingers.size === 2) {
      if (dragging) { act.button(1, false); dragging = false; }
      const m = mid();
      pinch = { dist: spread(), mx: m.x, my: m.y };
    }
  });

  target.addEventListener("pointermove", (e) => {
    const f = fingers.get(e.pointerId);
    if (!f) return;
    e.preventDefault();
    f.x = e.clientX;
    f.y = e.clientY;
    if (Math.hypot(f.x - f.x0, f.y - f.y0) > SLOP_PX) moved = true;
    if (fingers.size === 1 && moved && !longFired && maxFingers === 1) {
      cancelLong();
      const p = remote(f);
      if (!p) return;
      if (!dragging) {
        // The drag starts where the finger first touched.
        const p0 = view.toRemote(f.x0, f.y0);
        if (p0) act.move(p0);
        act.button(1, true);
        dragging = true;
      }
      act.move(p);
    } else if (fingers.size === 2 && pinch) {
      const m = mid();
      const d = spread();
      if (Math.abs(d - pinch.dist) > pinch.dist * 0.08 || view.zoom !== 1) {
        // Pinch: zoom around the fingers' midpoint, and follow it.
        view.setZoom(view.zoom * (d / pinch.dist), [m.x, m.y]);
      }
      const dx = m.x - pinch.mx, dy = m.y - pinch.my;
      if (view.pans()) {
        view.panBy(dx, dy);
      } else {
        wheelAcc.x += dx * 4;
        wheelAcc.y += dy * 4;
        const wx = Math.trunc(wheelAcc.x), wy = Math.trunc(wheelAcc.y);
        if (wx || wy) { act.wheel(wx, wy); wheelAcc = { x: wheelAcc.x - wx, y: wheelAcc.y - wy }; }
      }
      pinch = { dist: d, mx: m.x, my: m.y };
    }
  });

  const up = (e: PointerEvent) => {
    if (!fingers.delete(e.pointerId)) return;
    e.preventDefault();
    if (fingers.size === 2) {
      const m = mid();
      pinch = { dist: spread(), mx: m.x, my: m.y };
    }
    if (fingers.size > 0) return;
    cancelLong();
    if (dragging) { act.button(1, false); dragging = false; return; }
    if (moved || longFired || performance.now() - started > TAP_MS + 200) return;
    if (maxFingers === 1) { act.button(1, true); act.button(1, false); }
    else if (maxFingers === 2) { act.button(3, true); act.button(3, false); }
    else if (maxFingers === 3) { act.button(2, true); act.button(2, false); }
    else if (maxFingers >= 4) act.toggleKeyboard();
  };
  target.addEventListener("pointerup", up);
  target.addEventListener("pointercancel", up);
}
