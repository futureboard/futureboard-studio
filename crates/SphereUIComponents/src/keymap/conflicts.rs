use super::model::{KeymapConflict, KeymapScope, ResolvedKeyBinding};
use super::normalize::canonical_accel;
use std::collections::HashMap;

/// The bindings `keys` would collide with if bound to `action` in `scope`
/// (see [`KeymapScope::collides_with`]), leaving out `action`'s own binding in
/// that scope.
pub fn find_conflicts_for_binding(
    action: &str,
    keys: &[String],
    scope: KeymapScope,
    resolved: &[ResolvedKeyBinding],
) -> Vec<KeymapConflict> {
    let mut out = Vec::new();
    for key in keys {
        let Some(token) = canonical_accel(key) else {
            continue;
        };
        for existing in resolved {
            if existing.action == action && existing.scope == scope {
                continue;
            }
            if !existing.scope.collides_with(scope) {
                continue;
            }
            if !existing
                .keys
                .iter()
                .any(|k| canonical_accel(k).as_deref() == Some(token.as_str()))
            {
                continue;
            }
            out.push(KeymapConflict {
                keystroke: key.clone(),
                action: existing.action.clone(),
                action_label: existing.action.clone(),
                scope: existing.scope,
                source: existing.source,
            });
        }
    }
    out
}

pub fn annotate_row_conflicts(
    rows: &mut [super::model::KeymapRow],
    bindings: &[ResolvedKeyBinding],
) {
    let mut by_token: HashMap<String, Vec<&ResolvedKeyBinding>> = HashMap::new();
    for binding in bindings {
        for key in &binding.keys {
            if let Some(token) = canonical_accel(key) {
                by_token.entry(token).or_default().push(binding);
            }
        }
    }
    for row in rows.iter_mut() {
        row.is_conflict = false;
        row.conflict_with.clear();
        for key in &row.keystrokes {
            let Some(token) = canonical_accel(key) else {
                continue;
            };
            let Some(entries) = by_token.get(&token) else {
                continue;
            };
            for other in entries {
                if other.action == row.action_id && other.scope == row.scope {
                    continue;
                }
                if !other.scope.collides_with(row.scope) {
                    continue;
                }
                row.is_conflict = true;
                let label = format!("{} ({})", other.action, other.scope.label());
                if !row.conflict_with.contains(&label) {
                    row.conflict_with.push(label);
                }
            }
        }
    }
}
