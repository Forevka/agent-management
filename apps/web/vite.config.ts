import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The build output is embedded into the `fleet` binary (see crates/orchestrator/src/web.rs),
// so it goes somewhere the Rust build can find without reaching outside the workspace.
export default defineConfig({
  plugins: [react()],
  // `@/…` is the import path affectUI's components (and the shadcn CLI that installs them)
  // expect, so a component copied in from the registry compiles with no edits.
  // Root-relative on purpose: `node:url` would pull @types/node into a config that otherwise
  // needs no node types, and this app has no dependency it does not use.
  resolve: {
    alias: { '@': '/src' },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // One bundle each for app and vendor keeps the embedded asset list short and cacheable.
    // The design system is not in here: every component is our own source, in `src/components`.
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ['react', 'react-dom', 'react-router-dom'],
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
