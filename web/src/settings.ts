// Display settings, kept in this browser: what size the remote desktop
// should be (follow the window or a fixed resolution), its pixel ratio,
// which monitor on KDE/GNOME, and how the picture is shown.

export type ViewMode = "fit" | "fill" | "native";
export type Ratio = "auto" | 0.5 | 0.75 | 1 | 1.5 | 2;

export interface Settings {
  /** Follow the window, or a fixed resolution. */
  mode: "follow" | "fixed";
  fixedW: number;
  fixedH: number;
  /** Refresh rate asked for, Hz. */
  hz: number;
  /** Remote pixels per CSS pixel: "auto" is the device's, at most 2.
   *  Below 1 trades sharpness for speed: the same desktop in fewer pixels. */
  ratio: Ratio;
  /** KDE/GNOME: a virtual monitor of our own, or the real one. */
  target: "virtual" | "real";
  view: ViewMode;
}

export const PRESETS: [number, number][] = [[1280, 720], [1600, 900], [1920, 1080], [2560, 1440], [3840, 2160]];
export const RATES = [30, 60, 120];

const KEY = "quickremote.display";
const DEFAULTS: Settings = { mode: "follow", fixedW: 1920, fixedH: 1080, hz: 60, ratio: "auto", target: "virtual", view: "fit" };

export function load(): Settings {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) return { ...DEFAULTS, ...JSON.parse(raw) };
  } catch {
    // Private window or blocked storage: defaults.
  }
  return { ...DEFAULTS };
}

export function save(s: Settings): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    // Not kept; fine for this visit.
  }
}

/** The device's own ratio, as Auto would use it. */
export function deviceRatio(): number {
  return Math.min(Math.max(window.devicePixelRatio || 1, 1), 2);
}

export function effectiveRatio(s: Settings): number {
  return s.ratio === "auto" ? deviceRatio() : s.ratio;
}

/** A custom size from the user: even, within what the host takes. */
export function clampSize(w: number, h: number): [number, number] {
  const even = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, Math.round(v))) & ~1;
  return [even(w, 320, 7680), even(h, 200, 4320)];
}
