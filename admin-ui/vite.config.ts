import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// The SPA is mounted under `/admin` on the hub. `base` makes Vite
// emit asset URLs prefixed with `/admin/` so they resolve correctly
// when the bundle is served from there.
export default defineConfig({
  base: '/admin/',
  plugins: [
    react(),
    // /admin (no trailing slash) is unreachable under Vite's
    // `base: '/admin/'` — the dev server returns a "did you mean
    // /admin/?" hint instead of redirecting. Mirror what the hub
    // does in production and 301 to the canonical URL so deep-links
    // and address-bar typos just work in dev too.
    {
      name: 'redirect-admin-trailing-slash',
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url === '/admin') {
            res.statusCode = 301;
            res.setHeader('Location', '/admin/');
            res.end();
            return;
          }
          next();
        });
      },
    },
  ],
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  server: {
    port: 5173,
    proxy: {
      '/admin/api': {
        target: 'http://127.0.0.1:7101',
        changeOrigin: false,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 800,
  },
});
