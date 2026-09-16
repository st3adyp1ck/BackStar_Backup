import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwind from "@tailwindcss/vite";

// The frontend lives in ui/ so that src-tauri/ and crates/ sit alongside it
// rather than inside it.
export default defineConfig({
  root: "ui",
  plugins: [react(), tailwind()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "../dist", emptyOutDir: true, target: "chrome118" },
});
