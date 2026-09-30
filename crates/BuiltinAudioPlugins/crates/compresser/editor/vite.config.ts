import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import { viteSingleFile } from 'vite-plugin-singlefile'
import { fileURLToPath } from 'node:url'

// Ships as one self-contained HTML file for `builtin_ui_embed` / CEF. Fonts and
// assets must inline — no CDN or dev server at runtime.
export default defineConfig({
  root: fileURLToPath(new URL('.', import.meta.url)),
  plugins: [react(), tailwindcss(), viteSingleFile()],
  // Workspace dependencies such as Motion can otherwise resolve a different
  // React module instance from the editor. Hooks from that copy see a null
  // dispatcher and fail as soon as the first motion component mounts.
  resolve: {
    dedupe: ['react', 'react-dom'],
  },
  build: {
    target: 'es2022',
    assetsInlineLimit: 100_000_000,
    cssCodeSplit: false,
  },
})
