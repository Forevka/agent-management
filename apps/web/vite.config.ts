import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The build output is embedded into the `fleet` binary (see crates/orchestrator/src/web.rs),
// so it goes somewhere the Rust build can find without reaching outside the workspace.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // One bundle each for app and vendor keeps the embedded asset list short and cacheable.
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ['react', 'react-dom', 'react-router-dom', '@radix-ui/themes'],
        },
      },
    },
  },
  server: {
    // `npm run dev` against a local orchestrator: the API and the login route stay server-side.
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/login': 'http://127.0.0.1:8080',
    },
  },
})
