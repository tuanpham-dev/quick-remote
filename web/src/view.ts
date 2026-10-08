// How the remote picture sits in the page: a base scale from the view mode
// (Fit, Fill window, 100%), times the user's zoom, and a pan offset when
// the picture is larger than the stage. The single source for mapping
// page positions to remote pixels.

import type { ViewMode } from "./settings.ts";

export const ZOOM_MIN = 0.25;
export const ZOOM_MAX = 4;

export class View {
  mode: ViewMode = "fit";
  /** Remote pixels per CSS pixel at 100% (the pixel ratio). */
  ratio = 1;
  zoom = 1;
  /** Top-left of the picture in stage coordinates (CSS px). */
  x = 0;
  y = 0;
  scale = 1;
  onChange: () => void = () => {};

  constructor(private stage: HTMLElement, private canvas: HTMLCanvasElement) {
    new ResizeObserver(() => this.layout()).observe(stage);
  }

  /** CSS pixels per remote pixel before zoom. */
  private base(): number {
    const { width: sw, height: sh } = this.stage.getBoundingClientRect();
    const cw = this.canvas.width || 1, ch = this.canvas.height || 1;
    const fit = Math.min(sw / cw, sh / ch);
    const natural = 1 / this.ratio;
    // Fit shrinks a large picture but never enlarges past 100%.
    if (this.mode === "fit") return Math.min(fit, natural);
    if (this.mode === "fill") return fit;
    return natural;
  }

  /** Recomputes size and position (after a resize, a new picture size, a
   *  mode or zoom change), keeping the pan within bounds. */
  layout(): void {
    const { width: sw, height: sh } = this.stage.getBoundingClientRect();
    const prev = this.scale;
    this.scale = this.base() * this.zoom;
    const w = this.canvas.width * this.scale, h = this.canvas.height * this.scale;
    if (prev !== this.scale && prev > 0) {
      // Keep the stage's centre on the same remote point.
      const cx = (sw / 2 - this.x) / prev, cy = (sh / 2 - this.y) / prev;
      this.x = sw / 2 - cx * this.scale;
      this.y = sh / 2 - cy * this.scale;
    }
    this.x = w <= sw ? (sw - w) / 2 : Math.min(0, Math.max(sw - w, this.x));
    this.y = h <= sh ? (sh - h) / 2 : Math.min(0, Math.max(sh - h, this.y));
    const st = this.canvas.style;
    st.width = `${w}px`;
    st.height = `${h}px`;
    st.transform = `translate(${this.x}px, ${this.y}px)`;
    this.onChange();
  }

  /** Whether the picture is larger than the stage (it pans). */
  pans(): boolean {
    const { width: sw, height: sh } = this.stage.getBoundingClientRect();
    return this.canvas.width * this.scale > sw + 0.5 || this.canvas.height * this.scale > sh + 0.5;
  }

  setZoom(z: number, at?: [number, number]): void {
    const r = this.stage.getBoundingClientRect();
    const [px, py] = at ? [at[0] - r.left, at[1] - r.top] : [r.width / 2, r.height / 2];
    // The remote point under `at` stays under it.
    const rx = (px - this.x) / this.scale, ry = (py - this.y) / this.scale;
    this.zoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z));
    const next = this.base() * this.zoom;
    this.x = px - rx * next;
    this.y = py - ry * next;
    this.scale = next;
    this.layout();
  }

  panBy(dx: number, dy: number): void {
    this.x += dx;
    this.y += dy;
    this.layout();
  }

  /** The remote pixel under a page position, or null outside the picture. */
  toRemote(clientX: number, clientY: number): [number, number] | null {
    const r = this.stage.getBoundingClientRect();
    const x = (clientX - r.left - this.x) / this.scale;
    const y = (clientY - r.top - this.y) / this.scale;
    if (x < 0 || y < 0 || x >= this.canvas.width || y >= this.canvas.height) return null;
    return [Math.floor(x), Math.floor(y)];
  }

  /** The stage's size in CSS pixels. */
  stageSize(): [number, number] {
    const r = this.stage.getBoundingClientRect();
    return [r.width, r.height];
  }
}
