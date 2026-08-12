import { defineConfig } from 'vite';

// Tauri serves this build from disk: no remote origins, fixed dev port so the
// CSP and devUrl in tauri.conf.json stay exact (design doc §13).
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: false,
  },
});
