import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { fileURLToPath } from 'node:url'

// The server embeds `dist/` at compile time (see ../build.rs) and serves it
// itself. `bun run dev` serves the page from Vite instead and forwards the
// socket to a running livestage-server.
const server = process.env.LIVESTAGE_SERVER ?? '127.0.0.1:8730'

export default defineConfig({
  root: fileURLToPath(new URL('.', import.meta.url)),
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Nothing is fetched from the network at run time; keep files few.
    assetsInlineLimit: 8192,
  },
  server: {
    proxy: {
      '/ws': {
        target: `ws://${server}`,
        ws: true,
        changeOrigin: true,
        // The server only takes a browser socket from its own origin.
        configure: (proxy) => {
          proxy.on('proxyReqWs', (request) => {
            request.setHeader('origin', `http://${server}`)
          })
        },
      },
    },
  },
})
