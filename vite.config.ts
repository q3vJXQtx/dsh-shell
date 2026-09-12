import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 会在 dev 时注入该变量（跨设备调试用）
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  // 让 Tauri CLI 的输出不被 Vite 清屏冲掉
  clearScreen: false,
  server: {
    port: 1420,
    // 端口被占用时直接失败，而不是静默换端口——
    // 否则 tauri.conf.json 里的 devUrl 会对不上，表现为白屏
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: {
      // src-tauri 的改动由 cargo 自己监听，Vite 不必跟着重启
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    // WebView2 是 Chromium 内核，可以放心用较新的目标
    target: "chrome110",
    minify: "esbuild",
    sourcemap: false,
  },
});
