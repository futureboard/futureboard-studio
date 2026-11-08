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

## Crowdin

Translation project: [Futureboard Studio on Crowdin](https://crowdin.com/project/futureboard-studio).

`crowdin.yml` uploads the English catalog as a virtual `app.properties` source
and downloads translations back to `*/app.ftl`. The explicit `properties` type
is required because Fluent message IDs do not support the dotted keys used by
the native runtime. `escape_special_characters: 0` is set, but exported
translations still arrive with `\:` / `\=` escapes, so the runtime loader
unescapes `\:`, `\=`, `\#` and `\!`. Do not add `multilingual` to this mapping:
each downloaded file contains one locale.

The Crowdin integration also pushes its copy of the English source back to
`en-US/app.ftl` on `l10n_main`. When that copy lags behind `main`, merging
`l10n_main` silently deletes new English keys. The `i18n.rs` key-coverage test
catches that; take `main`'s `en-US/app.ftl` when resolving the merge.

To contribute a translation, open the Crowdin project, select a target language,
and translate the strings in `app.properties`. Keep placeholders such as
`{ $name }` and `{ $count }` unchanged. Synced translations are committed back
to the matching locale's `app.ftl` file.

## Maintainer pipeline

Optional helpers (maps under `scripts/translations/*.json` are gitignored):

1. Edit or regenerate English keys (`node scripts/generate-app-ftl.mjs` writes `en-US/app.ftl`).
2. Refresh English→locale JSON maps (from current FTL or `generate-locale-json.mjs`).
3. Apply maps: `node scripts/apply-locale-translations.mjs`.
4. Keep locale key sets aligned with `en-US` (same keys; no missing entries).

When adding UI copy, prefer a stable localization key in `en-US/app.ftl`, translate the other locales, then wire `i18n.tr("key")` at the render site.
