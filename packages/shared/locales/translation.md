# Locale translation status

Source of truth for shipped UI strings: committed `*/app.ftl` under this directory.
Despite the historical extension, these catalogs are flat `key = value` files
with dotted keys, not standard Fluent resources.

Locales: `en-US`, `ja-JP`, `lo-LA`, `th-TH`, `zh-CN` (selected by `settings.general.language`).
A new locale directory also needs a `Locale` variant in `i18n.rs` and a
`settings.language.<lang>` label in every catalog; `i18n.rs` tests fail otherwise.

## Runtime

Native UI loads FTL at compile time via `crates/SphereUIComponents/src/i18n.rs`.

Surfaces call `I18n::new(&language)` or `I18n::from_app(cx)`, then `tr` / `tr_vars` / `tr_menu`.

Menu labels use stable IDs from `native-menu.json` mapped as `menu.{id_with_dots_as_dashes}`.

## Contributing a translation

Translations live in this repository and are changed like any other file: edit
`<locale>/app.ftl` and open a merge request. There is no external translation
service; the committed catalogs are what ships.

- Translate the value after `=` only; never change a key.
- Keep placeholders such as `{ $name }` and `{ $count }` exactly as written
  (they may move within the sentence), and keep the literal `\n` line breaks.
- Values are written as-is: no `.properties`-style escaping (`\:`, `\=`, `\#`,
  `\!`). The `i18n.rs` tests reject a catalog that contains one.
- Every locale carries every `en-US` key; the `i18n.rs` key-coverage test fails
  otherwise. A string that is not translated yet keeps its English value.

## Maintainer pipeline

Optional helpers (maps under `scripts/translations/*.json` are gitignored):

1. Edit or regenerate English keys (`node scripts/generate-app-ftl.mjs` writes `en-US/app.ftl`).
2. Refresh English→locale JSON maps (from current FTL or `generate-locale-json.mjs`).
3. Apply maps: `node scripts/apply-locale-translations.mjs`.
4. Keep locale key sets aligned with `en-US` (same keys; no missing entries).

When adding UI copy, prefer a stable localization key in `en-US/app.ftl`, translate the other locales, then wire `i18n.tr("key")` at the render site.
