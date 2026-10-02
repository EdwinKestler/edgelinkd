import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  base: "/client/",
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/context": { target: "http://127.0.0.1:1888", changeOrigin: true },
      "/flows": { target: "http://127.0.0.1:1888", changeOrigin: true },
    },
  },
});
