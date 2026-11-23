import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'

// affectUI Pro is never committed (the repository is public): scripts/affectui.mjs writes the Pro
// files the app compiles to node_modules/.affectui/, which git has always ignored. So
// `@/components/ui/<name>` is the committed file in src/components/ui when there is one, and the
// Pro file otherwise. (tsconfig.json says the same to the type checker, with two paths.)
const affectuiPro: Plugin = {
  name: 'fleet-affectui-pro',
  enforce: 'pre',
  async resolveId(source, importer, options) {
    // The `@` alias below has already turned `@/…` into `/src/…` when this runs.
    const m = /^\/src\/components\/ui\/([a-z0-9-]+)$/.exec(source)
    if (!m) return null
    const own = await this.resolve(source, importer, { ...options, skipSelf: true })
    return own ?? this.resolve(`/node_modules/.affectui/${m[1]}.tsx`, importer, { ...options, skipSelf: true })
  },
}

// The build output is embedded into the `fleet` binary (see crates/orchestrator/src/web.rs),
// so it goes somewhere the Rust build can find without reaching outside the workspace.
export default defineConfig({
  plugins: [affectuiPro, react()],
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
    // A few bundles keep the embedded asset list short and cacheable. The design system is not in
    // here: every component is source (src/components, and affectUI Pro fetched at build time).
    // Icons get their own chunk: the free set is large, and only what is imported is kept.
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ['react', 'react-dom', 'react-router-dom'],
          icons: ['@hugeicons/react', '@hugeicons/core-free-icons'],
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
