import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 构建产物为纯静态文件（dist/），可放任意静态托管；
// 后端地址由 .env 的 VITE_API_BASE 注入
export default defineConfig({
  plugins: [react()],
});
