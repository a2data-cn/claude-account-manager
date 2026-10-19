import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 开发时固定端口；浏览器里单独调试前端时，/api 代理到 cam serve
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    proxy: { "/api": "http://127.0.0.1:8787" },
  },
  build: { target: "es2021", outDir: "dist", emptyOutDir: true },
});
