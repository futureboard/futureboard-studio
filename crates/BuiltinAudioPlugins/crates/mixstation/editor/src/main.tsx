import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { HashRouter, Route, Routes } from 'react-router-dom'
import App from './App'
import './styles.css'

// Browser debugging (`bun run dev`): no Futureboard host sits behind the page,
// so a preview host stands in for it. `import.meta.env.DEV` is `false` in the
// production build, which removes this branch and the module with it.
if (import.meta.env.DEV && window.location.protocol !== 'mikoplugin:') {
  void import('./dev/previewHost').then(({ startPreviewHost }) => startPreviewHost())
}

// HashRouter, not BrowserRouter: the page is served from the `mikoplugin:`
// custom scheme, where the History API has no real document URL to push against.
// The hash is the only navigation the CEF host can round-trip reliably.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <HashRouter>
      <Routes>
        <Route path="/instance/:instanceId" element={<App />} />
        <Route path="*" element={<App />} />
      </Routes>
    </HashRouter>
  </StrictMode>,
)
