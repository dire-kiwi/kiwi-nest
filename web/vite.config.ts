// The web UI's build. `npm run build` writes `dist/`, which the server embeds (rust-embed) and serves at `/`.
//
// `npm run dev` serves the UI with hot reloading and proxies the API (`/v1`) to a server: KN_SERVER, or the dev
// cluster's server on 127.0.0.1:7400. The server only takes a session cookie on requests from its own origin, so
// the proxy rewrites `Origin` to the server's (and `changeOrigin` sets `Host` to match). The cookie is `Secure`,
// which browsers accept on http://localhost.
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const server = process.env.KN_SERVER ?? 'http://127.0.0.1:7400'

export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist', emptyOutDir: true },
  server: {
    proxy: {
      '/v1': {
        target: server,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyReq', (req) => {
            if (req.getHeader('origin')) req.setHeader('origin', new URL(server).origin)
          })
        },
      },
    },
  },
})
