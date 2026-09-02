import { resolve } from 'node:path';

import { defineConfig } from 'vite';

// Tauri serves this build from disk: no remote origins, fixed dev port so the
// CSP and devUrl in tauri.conf.json stay exact (design doc §13).
//
// Three entries, because each Tauri window loads its own page: `index.html`
// is the consent/invite/status screen of the main window, `view.html` is one
// remote-view window per watched host, and `hostbar.html` is the host's
// always-on-top session bar (ADR 0055). Keeping them separate is also what
// keeps the view window's bundle free of the main screen's code, and the
// bar's — which is on screen over everything else while a session runs —
// free of both.
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
        hostbar: resolve(import.meta.dirname, 'hostbar.html'),
      },
    },
  },
});
