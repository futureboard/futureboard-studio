// Futureboard Studio's default theme, as CSS custom properties.
//
// Studio's colours live in one file, packages/shared/themes/Default.json,
// which crates/SphereUIComponents/src/theme.rs embeds and resolves its
// `Colors::*` tokens from. This plug-in reads the same file at build time
// (and on every change under `bun run dev`) and turns each token into a
// custom property, so the page cannot drift from Studio:
//
//   tokens.surface.panelAlt   -> --fb-surface-panel-alt
//   tokens.tab.text_muted     -> --fb-tab-text-muted
//   trackColors[0]            -> --fb-track-color-1
//
// The page imports them as `virtual:futureboard-theme.css`; src/styles.css
// maps them to the roles the UI uses. Nothing is fetched at run time — the
// properties are compiled into the page's stylesheet.

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import type { Plugin } from 'vite'

export const THEME_PATH = fileURLToPath(new URL('../../../../packages/shared/themes/Default.json', import.meta.url))

const PUBLIC_ID = 'virtual:futureboard-theme.css'
const RESOLVED_ID = '\0' + PUBLIC_ID
const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i

interface ThemeFile {
  id?: string
  version?: string
  tokens?: Record<string, unknown>
  trackColors?: unknown[]
}

/** `panelAlt` → `panel-alt`, `text_muted` → `text-muted`. */
function kebab(key: string): string {
  return key
    .replace(/_/g, '-')
    .replace(/([a-z0-9])([A-Z])/g, '$1-$2')
    .toLowerCase()
}

function color(path: string, value: unknown): string {
  if (typeof value !== 'string' || !HEX.test(value)) {
    throw new Error(`futureboard-theme: ${path} is not a hex colour: ${JSON.stringify(value)}`)
  }
  return value.toLowerCase()
}

/** Every colour in the theme, in file order, as `[property, value]`. */
export function themeProperties(file: ThemeFile = readTheme()): [string, string][] {
  const out: [string, string][] = []
  const walk = (node: unknown, path: string[]) => {
    if (node && typeof node === 'object' && !Array.isArray(node)) {
      for (const [key, value] of Object.entries(node)) walk(value, [...path, key])
    } else {
      out.push([`--fb-${path.map(kebab).join('-')}`, color(path.join('.'), node)])
    }
  }
  walk(file.tokens ?? {}, [])
  ;(file.trackColors ?? []).forEach((value, i) => out.push([`--fb-track-color-${i + 1}`, color(`trackColors[${i}]`, value)]))
  const seen = new Set<string>()
  for (const [name] of out) {
    if (seen.has(name)) throw new Error(`futureboard-theme: two tokens map to ${name}`)
    seen.add(name)
  }
  return out
}

function readTheme(): ThemeFile {
  return JSON.parse(readFileSync(THEME_PATH, 'utf8')) as ThemeFile
}

function stylesheet(): string {
  const file = readTheme()
  const lines = themeProperties(file).map(([name, value]) => `  ${name}: ${value};`)
  return `/* Generated from packages/shared/themes/Default.json (${file.id ?? '?'} ${file.version ?? ''}) by futureboard-theme.ts. */\n:root {\n${lines.join('\n')}\n}\n`
}

export function futureboardTheme(): Plugin {
  return {
    name: 'futureboard-theme',
    resolveId(id) {
      return id === PUBLIC_ID ? RESOLVED_ID : undefined
    },
    load(id) {
      if (id !== RESOLVED_ID) return undefined
      this.addWatchFile(THEME_PATH)
      return stylesheet()
    },
    configureServer(server) {
      // A theme edit while `bun run dev` runs: rebuild the sheet, reload.
      server.watcher.add(THEME_PATH)
      server.watcher.on('change', (path) => {
        if (path.replace(/\\/g, '/') !== THEME_PATH.replace(/\\/g, '/')) return
        const module = server.moduleGraph.getModuleById(RESOLVED_ID)
        if (module) server.moduleGraph.invalidateModule(module)
        server.ws.send({ type: 'full-reload' })
      })
    },
    // The browser chrome's colour (a phone's status bar): Studio's title bar.
    transformIndexHtml() {
      const titlebar = themeProperties().find(([name]) => name === '--fb-surface-titlebar')
      if (!titlebar) throw new Error('futureboard-theme: the theme has no surface.titlebar')
      return [{ tag: 'meta', attrs: { name: 'theme-color', content: titlebar[1] }, injectTo: 'head' }]
    },
  }
}
