import path from 'node:path'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// The Rust API (`wh serve`) listens on 127.0.0.1:7070 by default.
const api = process.env.WH_API ?? 'http://127.0.0.1:7070'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { '@': path.resolve(import.meta.dirname, './src') },
  },
  build: { chunkSizeWarningLimit: 800 },
  server: {
    proxy: { '/api': { target: api, changeOrigin: true } },
  },
})
