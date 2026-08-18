import { resolve } from 'node:path';

import { defineConfig } from 'vite';

// Tauri serves this build from disk: no remote origins, fixed dev port so the
// CSP and devUrl in tauri.conf.json stay exact (design doc §13).
//
// Two entries, because each Tauri window loads its own page: `index.html` is
// the consent/invite/status screen of the main window, `view.html` is one
// remote-view window per watched host. Keeping them separate is also what keeps
// the view window's bundle free of the main screen's code.
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
    rollupOptions: {
      input: {
        main: resolve(import.meta.dirname, 'index.html'),
        view: resolve(import.meta.dirname, 'view.html'),
      },
    },
  },
});
