import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App.tsx'
import './styles.css'

// Browser debugging (`bun run dev`): no Futureboard host sits behind the page,
// so a preview host stands in for it. `import.meta.env.DEV` is `false` in the
// production build, which removes this branch and the module with it.
if (import.meta.env.DEV && window.location.protocol !== 'mikoplugin:') {
  void import('./dev/previewHost').then(({ startPreviewHost }) => startPreviewHost())
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
