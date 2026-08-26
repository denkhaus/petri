//! GitHub Actions matrix expansion, as one pure function.
//!
//! Static and expression-valued matrices go through the same code: the frontend
//! lowers `strategy.matrix` to `matrix_combinations(<object>)`, and the engine
//! evaluates it at firing time whether the object was a literal or came from
//! `fromJSON(needs.x.outputs.y)`. One algorithm means the two cannot disagree.
//!
//! The documented rules:
//!
//! 1. The cartesian product of every key except `include` and `exclude`, in key
//!    order, with the first key varying slowest.
//! 2. `exclude` removes every product combination that matches all of an exclude
//!    entry's keys. Excludes are applied before includes, so an include can add a
//!    combination back.
//! 3. For each `include` entry, in order: if its *original-key* values conflict with
//!    none of a product combination's original values, its other keys are added to
//!    that combination (overwriting values earlier includes added, never original
//!    ones). If it could be added to no product combination, it becomes a new
//!    combination. Only product combinations are candidates — a combination an
//!    earlier include *created* is never extended by a later one, which is why the
//!    documented example ends with both `{fruit: banana}` and
//!    `{fruit: banana, animal: cat}`.
//!
//! A matrix with only `include` yields one combination per entry.

use serde_json::{Map, Value};

/// Expand a matrix object into its list of combinations.
///
/// Anything that is not an object yields an empty list rather than an error: a
/// missing or null matrix means "no legs", which is what a workflow author expects.
pub fn combinations(matrix: &Value) -> Vec<Value> {
    let Some(map) = matrix.as_object() else {
        return Vec::new();
    };

    // Original keys, in order, each with its list of values.
    let axes: Vec<(&String, Vec<Value>)> = map
        .iter()
        .filter(|(k, _)| k.as_str() != "include" && k.as_str() != "exclude")
        .map(|(k, v)| {
            let values = match v {
                Value::Array(items) => items.clone(),
                // A scalar axis is a one-value axis.
                other => vec![other.clone()],
            };
            (k, values)
        })
        .collect();
    let original_keys: Vec<&str> = axes.iter().map(|(k, _)| k.as_str()).collect();

    let mut combos: Vec<Map<String, Value>> = if axes.is_empty() {
        Vec::new()
    } else {
        let mut acc: Vec<Map<String, Value>> = vec![Map::new()];
        for (key, values) in &axes {
            let mut next = Vec::with_capacity(acc.len() * values.len().max(1));
            for combo in &acc {
                for value in values {
                    let mut extended = combo.clone();
                    extended.insert((*key).clone(), value.clone());
                    next.push(extended);
                }
            }
            acc = next;
        }
        acc
    };

    if let Some(Value::Array(excludes)) = map.get("exclude") {
        for exclude in excludes.iter().filter_map(Value::as_object) {
            combos.retain(|combo| {
                !exclude
                    .iter()
                    .all(|(k, v)| combo.get(k).is_some_and(|have| have == v))
            });
        }
    }

    let mut added: Vec<Map<String, Value>> = Vec::new();
    if let Some(Value::Array(includes)) = map.get("include") {
        for include in includes.iter().filter_map(Value::as_object) {
            let mut applied = false;
            for combo in combos.iter_mut() {
                // An include may not overwrite an original value.
                let conflicts = include.iter().any(|(k, v)| {
                    original_keys.contains(&k.as_str())
                        && combo.get(k).is_some_and(|have| have != v)
                });
                if conflicts {
                    continue;
                }
                for (k, v) in include {
                    if !original_keys.contains(&k.as_str()) {
                        combo.insert(k.clone(), v.clone());
                    }
                }
                applied = true;
            }
            if !applied {
                added.push(include.clone());
            }
        }
    }

    combos.into_iter().chain(added).map(Value::Object).collect()
}
