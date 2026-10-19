// Generate `packages/shared/generated/native-menu.json` from the shared menu
// definition (`packages/shared/src/menu/menuItems.ts`): a Rust-friendly JSON
// manifest the native crate (`crates/SphereUIComponents`) embeds via
// `include_str!` and parses at startup.
//
// Run with Bun so we can dynamically import the .ts module without an
// external TypeScript loader. Bun executes plain `.mjs` files natively.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const workspaceRoot = path.resolve(__dirname, "..");
const sourcePath = path.join(
  workspaceRoot,
  "packages",
  "shared",
  "src",
  "menu",
  "menuItems.ts",
);

// ── Native Rust JSON manifest ─────────────────────────────────────────
const nativeJsonPath = path.join(
  workspaceRoot,
  "packages",
  "shared",
  "generated",
  "native-menu.json",
);
fs.mkdirSync(path.dirname(nativeJsonPath), { recursive: true });

const sourceUrl = pathToFileURL(sourcePath).href;
let menus;
try {
  const mod = await import(sourceUrl);
  menus = mod.APP_MENUS;
} catch (err) {
  console.error(
    `[sync-shared-menu] failed to import ${rel(sourcePath)}: ${err && err.message ? err.message : err}`,
  );
  console.error(
    "[sync-shared-menu] hint: this script must be run with Bun, e.g. `bun scripts/sync-shared-menu.mjs`",
  );
  process.exit(1);
}

if (!Array.isArray(menus)) {
  console.error(
    `[sync-shared-menu] APP_MENUS export from ${rel(sourcePath)} is not an array`,
  );
  process.exit(1);
}

const manifest = {
  version: 1,
  generatedAt: new Date().toISOString(),
  menus: menus.map(toRustMenu),
};

let itemCount = 0;
const walk = (items) => {
  for (const it of items) {
    itemCount += 1;
    if (it.children) walk(it.children);
  }
};
for (const m of manifest.menus) walk(m.items);

fs.writeFileSync(nativeJsonPath, JSON.stringify(manifest, null, 2) + "\n", "utf8");
console.log(
  `[sync-shared-menu] generated ${rel(nativeJsonPath)}: ${manifest.menus.length} menus, ${itemCount} items`,
);

// ── Helpers ──────────────────────────────────────────────────────────────

function toRustMenu(group) {
  return {
    id: String(group.id),
    label: String(group.label),
    items: (group.children ?? []).map(toRustItem),
  };
}

function toRustItem(item) {
  const t = item.type ?? "item";
  if (t === "separator") {
    return {
      id: String(item.id),
      kind: "separator",
      enabled: true,
      visible: true,
    };
  }
  if (t === "submenu") {
    return {
      id: String(item.id),
      kind: "submenu",
      label: String(item.label),
      icon: item.icon ?? null,
      shortcut: null,
      command: null,
      enabled: item.enabled !== false,
      visible: true,
      children: (item.children ?? []).map(toRustItem),
    };
  }
  // Normal item — checkbox if the source declares a `checked` field, otherwise normal.
  const kind = Object.prototype.hasOwnProperty.call(item, "checked")
    ? "checkbox"
    : "normal";
  return {
    id: String(item.id),
    kind,
    label: String(item.label),
    icon: item.icon ?? null,
    shortcut: item.accelerator ?? null,
    command: item.action ?? null,
    role: item.role ?? null,
    enabled: item.enabled !== false,
    visible: true,
    checked: kind === "checkbox" ? Boolean(item.checked) : false,
    danger: Boolean(item.danger),
    description: item.description ?? null,
    children: [],
  };
}

function rel(p) {
  return path.relative(workspaceRoot, p).replace(/\\/g, "/");
}
