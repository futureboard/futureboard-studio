/**
 * sync-version.mjs
 *
 * Single source of truth: repoRoot/version.json
 *
 * Sync targets (every file that carries the product version):
 * - Cargo: the root `[workspace.package] version` (inherited by every crate
 *   declaring `version.workspace = true`) and every first-party `Cargo.toml`
 *   with an explicit `[package] version` — apps/native/*, crates/* and the
 *   built-in plug-in crates. Vendored forks (crates/gpui, crates/Ara2Bridge,
 *   external/) keep their own upstream versions.
 * - Cargo.lock: the `[[package]]` entries of those workspace members, so a
 *   `--locked` build after a bump does not fail on a stale lockfile.
 * - packages/shared/app/windows/app.rc: FILEVERSION / PRODUCTVERSION tuples
 *   and the FileVersion / ProductVersion strings.
 * - packaging/windows/installer.iss and the Professional installer (when its
 *   private checkout is present): `#define MyAppVersion`.
 * - .github/workflows/release.yml: the release tag the dispatch form is
 *   prefilled with.
 * - packaging/aur/PKGBUILD and .SRCINFO: `pkgver` (AUR forbids `-`, so a
 *   pre-release such as `2026.9.1-beta1.2` becomes `2026.9.1_beta1.2`),
 *   `_appver` (the exact release asset name), and the .SRCINFO source URL
 *   (canonical repo, versioned tag — not the moving nightly release).
 *   The sha256 stays put; the release workflow writes it from the AppImage.
 * - packaging/native/Info.plist: CFBundleShortVersionString / CFBundleVersion
 *   (Apple accepts numeric dot-separated components only, so the pre-release
 *   suffix is dropped there; the full version stays in the binary).
 *
 * Usage:
 *   node scripts/sync-version.mjs                    # sync from version.json
 *   node scripts/sync-version.mjs --check            # fail if out of sync
 *   node scripts/sync-version.mjs --version 1.2.3    # sync an explicit version
 *   node scripts/sync-version.mjs --version 1.2.3 --check
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

// Resolve repo root from this script location, not from process.cwd(),
// so CI steps with different working directories still behave correctly.
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const checkOnly = process.argv.includes("--check");
const versionArgIdx = process.argv.indexOf("--version");
const versionOverride =
  versionArgIdx !== -1 ? process.argv[versionArgIdx + 1] : undefined;
if (versionArgIdx !== -1 && !versionOverride) {
  throw new Error("--version requires a value");
}

function readJson(p) {
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

const versionPath = path.join(repoRoot, "version.json");
if (!fs.existsSync(versionPath)) {
  throw new Error(`Missing ${versionPath}`);
}

const { version: jsonVersion } = readJson(versionPath);
const version = versionOverride ?? jsonVersion;
if (typeof version !== "string" || version.length < 1) {
  throw new Error(`Invalid version: expected a non-empty string`);
}
// Cargo needs SemVer: MAJOR.MINOR.PATCH with an optional -pre / +build.
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(
    `Invalid version "${version}": expected MAJOR.MINOR.PATCH[-prerelease][+build]`,
  );
}

// ── Derived forms ────────────────────────────────────────────────────────────

/** `2026.9.1-beta1.2` -> `["2026", "9", "1"]` */
const numericCore = version.match(/^(\d+)\.(\d+)\.(\d+)/).slice(1, 4);
/** Windows VERSIONINFO tuple: four comma-separated integers. */
const rcTuple = `${numericCore.join(",")},0`;
/** Apple bundle fields: numeric dot-separated components only. */
const bundleVersion = numericCore.join(".");
/** AUR `pkgver`: no hyphens allowed. */
const aurPkgver = version.replace(/-/g, "_");

// ── Helpers ──────────────────────────────────────────────────────────────────

/**
 * Replace the first match of `re` in `text` with `replacement`, requiring a
 * match. Every pattern here captures its prefix as group 1 and the current
 * value as group 2; the value is returned as `from` for the change log.
 */
function replaceRequired(text, re, replacement, what, file) {
  const match = text.match(re);
  if (!match) {
    throw new Error(`${file}: no ${what} found`);
  }
  return { text: text.replace(re, replacement), from: match[2] };
}

/** Escape a string for use inside a RegExp. */
function escapeRe(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Line-oriented TOML edit: rewrite `version = "..."` only inside the named
 * sections (`package` / `workspace.package`), never inside `[dependencies]`
 * where a version is a requirement on someone else's crate.
 */
function replaceTomlSectionVersion(text, sections, newVersion) {
  const lines = text.split("\n");
  let section = null;
  let from = null;
  let changed = false;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const header = line.match(/^\s*\[\[?([^\]]+)\]\]?\s*(?:#.*)?$/);
    if (header) {
      section = header[1].trim();
      continue;
    }
    if (!sections.includes(section)) continue;
    const m = line.match(/^(\s*version\s*=\s*")([^"]*)(".*)$/);
    if (m) {
      from = m[2];
      if (m[2] !== newVersion) {
        lines[i] = `${m[1]}${newVersion}${m[3]}`;
        changed = true;
      }
      // One version per section; keep scanning for other sections.
      section = null;
    }
  }
  return { text: lines.join("\n"), from, changed };
}

/** `[package] name` of a manifest, or null when it is a workspace root only. */
function tomlPackageName(text) {
  const lines = text.split("\n");
  let section = null;
  for (const line of lines) {
    const header = line.match(/^\s*\[\[?([^\]]+)\]\]?\s*(?:#.*)?$/);
    if (header) {
      section = header[1].trim();
      continue;
    }
    if (section !== "package") continue;
    const m = line.match(/^\s*name\s*=\s*"([^"]*)"/);
    if (m) return m[1];
  }
  return null;
}

/** Whether a manifest's `[package]` inherits the workspace version. */
function tomlInheritsWorkspaceVersion(text) {
  return /^\s*version\.workspace\s*=\s*true\s*$/m.test(text);
}

// ── Cargo discovery ──────────────────────────────────────────────────────────

/** Directories that are not Futureboard's own code: never touched. */
const SKIP_DIR_NAMES = new Set([
  ".git",
  "node_modules",
  "target",
  "out",
  "build",
  "dist",
]);
const SKIP_DIR_PATHS = [
  "external",
  // Vendored forks keep their upstream versions.
  path.join("crates", "gpui"),
  path.join("crates", "Ara2Bridge"),
  path.join("crates", "collections"),
  // Developer tooling and third-party extension scaffolds are not the product.
  "xtask",
  "extensions",
];

function* walkCargoManifests(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    const rel = path.relative(repoRoot, full);
    if (entry.isDirectory()) {
      if (SKIP_DIR_NAMES.has(entry.name)) continue;
      if (SKIP_DIR_PATHS.some((skip) => rel === skip || rel.startsWith(skip + path.sep)))
        continue;
      yield* walkCargoManifests(full);
    } else if (entry.name === "Cargo.toml") {
      yield full;
    }
  }
}

/**
 * Package names of every `path = "..."` dependency declared by `manifest`
 * that resolves *outside* the repository. Such a crate is built from the
 * other checkout, with that checkout's version, even if a same-named copy
 * sits inside this repo (e.g. `fbmx-runtime` comes from `../dsp`), so its
 * Cargo.lock entry must be left to cargo.
 */
function externalPathDependencyNames(manifest, text) {
  const names = new Set();
  const dir = path.dirname(manifest);
  const re =
    /^\s*([A-Za-z0-9_-]+)\s*=\s*\{([^}]*)\}/gm;
  for (const match of text.matchAll(re)) {
    const [, key, body] = match;
    const pathSpec = body.match(/\bpath\s*=\s*"([^"]+)"/)?.[1];
    if (!pathSpec) continue;
    const resolved = path.resolve(dir, pathSpec);
    const rel = path.relative(repoRoot, resolved);
    const outside = rel.startsWith("..") || path.isAbsolute(rel);
    if (!outside) continue;
    // `package = "real-name"` renames the key; prefer the manifest's own name.
    const renamed = body.match(/\bpackage\s*=\s*"([^"]+)"/)?.[1];
    let name = renamed ?? key;
    const externalManifest = path.join(resolved, "Cargo.toml");
    if (fs.existsSync(externalManifest)) {
      name = tomlPackageName(fs.readFileSync(externalManifest, "utf8")) ?? name;
    }
    names.add(name);
  }
  return names;
}

/**
 * Build one target per first-party manifest that declares a version, and
 * collect the names of every workspace member so Cargo.lock can follow.
 */
function cargoTargets() {
  const targets = [];
  const memberNames = new Set();
  const externalNames = new Set();
  for (const manifest of walkCargoManifests(repoRoot)) {
    const rel = path.relative(repoRoot, manifest).split(path.sep).join("/");
    const text = fs.readFileSync(manifest, "utf8");
    const name = tomlPackageName(text);
    const isRoot = manifest === path.join(repoRoot, "Cargo.toml");
    const sections = isRoot ? ["workspace.package", "package"] : ["package"];
    const probe = replaceTomlSectionVersion(text, sections, version);
    for (const external of externalPathDependencyNames(manifest, text)) {
      externalNames.add(external);
    }
    if (name && (probe.from !== null || tomlInheritsWorkspaceVersion(text))) {
      memberNames.add(name);
    }
    if (probe.from === null) continue; // inherits, or no version at all
    targets.push({
      name: rel,
      path: manifest,
      transform: (current) => {
        const result = replaceTomlSectionVersion(current, sections, version);
        return { text: result.text, from: result.from, to: version };
      },
    });
  }
  for (const external of externalNames) {
    memberNames.delete(external);
  }
  return { targets, memberNames };
}

/**
 * Cargo.lock: every `[[package]]` block naming a workspace member (path
 * packages have no `source =` line) takes the new version.
 */
function cargoLockTransform(memberNames) {
  return (current) => {
    const blocks = current.split("\n[[package]]\n");
    let from = null;
    const next = blocks
      .map((block, index) => {
        if (index === 0) return block; // header before the first package
        const name = block.match(/^name = "([^"]*)"/m)?.[1];
        if (!name || !memberNames.has(name)) return block;
        if (/^source = /m.test(block)) return block; // a registry crate of the same name
        const m = block.match(/^version = "([^"]*)"$/m);
        if (!m) return block;
        from = from ?? m[1];
        return block.replace(/^version = "([^"]*)"$/m, `version = "${version}"`);
      })
      .join("\n[[package]]\n");
    return { text: next, from, to: version };
  };
}

// ── Targets ──────────────────────────────────────────────────────────────────

const { targets: cargo, memberNames } = cargoTargets();

const targets = [
  ...cargo,
  {
    name: "Cargo.lock",
    path: path.join(repoRoot, "Cargo.lock"),
    optional: true,
    transform: cargoLockTransform(memberNames),
  },
  {
    name: "packages/shared/app/windows/app.rc",
    path: path.join(repoRoot, "packages", "shared", "app", "windows", "app.rc"),
    transform: (current) => {
      const file = "app.rc";
      let text = current;
      let from;
      ({ text, from } = replaceRequired(
        text,
        /^(\s*FILEVERSION\s+)([\d,]+)/m,
        `$1${rcTuple}`,
        "FILEVERSION",
        file,
      ));
      ({ text } = replaceRequired(
        text,
        /^(\s*PRODUCTVERSION\s+)([\d,]+)/m,
        `$1${rcTuple}`,
        "PRODUCTVERSION",
        file,
      ));
      ({ text } = replaceRequired(
        text,
        /(VALUE\s+"FileVersion",\s*")([^"\\]*)(\\0")/,
        `$1${version}$3`,
        "FileVersion",
        file,
      ));
      ({ text } = replaceRequired(
        text,
        /(VALUE\s+"ProductVersion",\s*")([^"\\]*)(\\0")/,
        `$1${version}$3`,
        "ProductVersion",
        file,
      ));
      return { text, from, to: `${rcTuple} / ${version}` };
    },
  },
  {
    name: "packaging/windows/installer.iss",
    path: path.join(repoRoot, "packaging", "windows", "installer.iss"),
    transform: (current) => {
      const r = replaceRequired(
        current,
        /^(#define MyAppVersion ")([^"]*)(")/m,
        `$1${version}$3`,
        "#define MyAppVersion",
        "installer.iss",
      );
      return { text: r.text, from: r.from, to: version };
    },
  },
  {
    // Private Professional checkout; absent on a public clone.
    name: "crates/ExclusiveEdition/packaging/windows/installer-pro.iss",
    path: path.join(
      repoRoot,
      "crates",
      "ExclusiveEdition",
      "packaging",
      "windows",
      "installer-pro.iss",
    ),
    optional: true,
    transform: (current) => {
      const r = replaceRequired(
        current,
        /^(#define MyAppVersion ")([^"]*)(")/m,
        `$1${version}$3`,
        "#define MyAppVersion",
        "installer-pro.iss",
      );
      return { text: r.text, from: r.from, to: version };
    },
  },
  {
    // The dispatch form for a release build is prefilled with this default, so
    // a stale one attaches a freshly built installer to whatever tag was
    // current months ago. Syncing it here is what keeps the release trigger
    // honest without anybody remembering to.
    name: ".github/workflows/release.yml",
    path: path.join(repoRoot, ".github", "workflows", "release.yml"),
    transform: (current) => {
      const r = replaceRequired(
        current,
        /^(        default: ")([^"]*)(")$/m,
        `$1${version}$3`,
        "release tag default",
        "release.yml",
      );
      return { text: r.text, from: r.from, to: version };
    },
  },
  {
    name: "packaging/aur/PKGBUILD",
    path: path.join(repoRoot, "packaging", "aur", "PKGBUILD"),
    transform: (current) => {
      let { text, from } = replaceRequired(
        current,
        /^(pkgver=)(\S*)$/m,
        `$1${aurPkgver}`,
        "pkgver",
        "PKGBUILD",
      );
      // The release asset keeps the exact version (hyphen included), which
      // AUR's pkgver cannot carry, so it lives in its own variable.
      if (/^_appver=/m.test(text)) {
        text = text.replace(/^(_appver=)(\S*)$/m, `$1${version}`);
      } else {
        text = text.replace(/^(pkgver=\S*)$/m, `$1\n_appver=${version}`);
      }
      return { text, from, to: aurPkgver };
    },
  },
  {
    name: "packaging/aur/.SRCINFO",
    path: path.join(repoRoot, "packaging", "aur", ".SRCINFO"),
    optional: true,
    transform: (current) => {
      const r = replaceRequired(
        current,
        /^(\tpkgver = )(\S*)$/m,
        `$1${aurPkgver}`,
        "pkgver",
        ".SRCINFO",
      );
      // Expand the versioned asset URL. Matching the previous pkgver string
      // missed this line whenever the filename still used hyphens (AUR pkgver
      // stores the same version with `_`) or still pointed at nightly.
      const asset = `Futureboard.Studio-${version}-x86_64.AppImage`;
      const source = `\tsource_x86_64 = ${asset}::https://github.com/futureboard/futureboard-studio/releases/download/${version}/${asset}`;
      if (!/^\tsource_x86_64 = /m.test(r.text)) {
        throw new Error(".SRCINFO is missing source_x86_64");
      }
      const text = r.text.replace(/^\tsource_x86_64 = .*$/m, source);
      return { text, from: r.from, to: aurPkgver };
    },
  },
  {
    name: "packaging/native/Info.plist",
    path: path.join(repoRoot, "packaging", "native", "Info.plist"),
    transform: (current) => {
      let text = current;
      let from;
      for (const key of ["CFBundleShortVersionString", "CFBundleVersion"]) {
        const re = new RegExp(
          `(<key>${key}</key>\\s*<string>)([^<]*)(</string>)`,
        );
        const r = replaceRequired(text, re, `$1${bundleVersion}$3`, key, "Info.plist");
        text = r.text;
        from = from ?? r.from;
      }
      return { text, from, to: bundleVersion };
    },
  },
];

// ── Apply ────────────────────────────────────────────────────────────────────

let dirty = false;
for (const target of targets) {
  if (!fs.existsSync(target.path)) {
    if (target.optional) {
      console.log(`[sync-version] skipped (absent): ${target.name}`);
      continue;
    }
    throw new Error(`Missing sync target: ${target.name}`);
  }
  const currentText = fs.readFileSync(target.path, "utf8");
  const { text: nextText, from, to } = target.transform(currentText);
  if (nextText === currentText) {
    console.log(`[sync-version] ok: ${target.name}`);
    continue;
  }
  dirty = true;
  const details = from !== undefined && from !== null ? ` (${from} -> ${to})` : "";
  if (checkOnly) {
    console.log(`[sync-version] out of sync: ${target.name}${details}`);
  } else {
    // Byte-for-byte rewrite of the matched fields only: line endings and
    // formatting around them are preserved.
    fs.writeFileSync(target.path, nextText, "utf8");
    console.log(`[sync-version] updated: ${target.name}${details}`);
  }
}

if (checkOnly && dirty) {
  console.error(
    `[sync-version] ERROR: version mismatch. Run: node scripts/sync-version.mjs`,
  );
  process.exit(1);
}
