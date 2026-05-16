import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

export default defineConfig({
  base: '/app/',
  plugins: [
    react(),
    {
      name: 'redirect-app-trailing-slash',
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url === '/app') {
            res.statusCode = 301;
            res.setHeader('Location', '/app/');
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
    port: 5174,
    proxy: {
      '/app/api': {
        target: 'http://127.0.0.1:7101',
        changeOrigin: false,
      },
      '/v1/pty/ws': {
        target: 'ws://127.0.0.1:7100',
        ws: true,
        changeOrigin: false,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
  },
});
