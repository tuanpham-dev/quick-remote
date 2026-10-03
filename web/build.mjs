// Builds web/dist (embedded into qr-host): the page, the bundle and the
// WebAssembly core. Run `cargo build --release -p qr-wasm --target
// wasm32-unknown-unknown` and wasm-bindgen first (scripts/build-web.sh).
import { build } from "esbuild";
import { copyFileSync, mkdirSync } from "node:fs";

mkdirSync("dist", { recursive: true });
await build({
  entryPoints: ["src/main.ts"],
  bundle: true,
  format: "esm",
  target: "es2022",
  outfile: "dist/main.js",
  minify: true,
  sourcemap: false,
  external: ["*.wasm"],
});
copyFileSync("index.html", "dist/index.html");
copyFileSync("src/wasm/qr_wasm_bg.wasm", "dist/qr_wasm_bg.wasm");
console.log("built web/dist");
