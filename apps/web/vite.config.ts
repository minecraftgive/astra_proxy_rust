import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": {
        target: process.env.ASTRA_WEB_DEV_UPSTREAM ?? "http://127.0.0.1:8787",
        // The development gateway uses this frontend's public_origin as well.
        changeOrigin: false,
      },
    },
  },
  build: { sourcemap: false },
});
