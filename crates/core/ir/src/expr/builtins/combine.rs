//! Combinators over lists of records, as pure functions.
//!
//! These are the pieces a frontend composes to expand a build matrix — any
//! frontend's matrix. A cartesian product, removing records that match a
//! partial record, and merging partial records into the ones they match. None
//! of them knows what a `strategy.matrix`, an `exclude:` or an `adjustments:`
//! is; the frontend that does spells its format's rule out of these.

use serde_json::{Map, Value};

/// Every combination of one value from each key, in key order, the first key
/// varying slowest. Anything that is not an object yields an empty list; a
/// scalar value is a one-element axis.
pub fn cartesian(axes: &Value) -> Vec<Value> {
    let Some(map) = axes.as_object() else {
        return Vec::new();
    };
    if map.is_empty() {
        return Vec::new();
    }
    let mut acc: Vec<Map<String, Value>> = vec![Map::new()];
    for (key, value) in map {
        let values: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        let mut next = Vec::with_capacity(acc.len() * values.len().max(1));
        for combo in &acc {
            for value in &values {
                let mut extended = combo.clone();
                extended.insert(key.clone(), (*value).clone());
                next.push(extended);
            }
        }
        acc = next;
    }
    acc.into_iter().map(Value::Object).collect()
}

/// Whether every key of `partial` is present in `record` with an equal value.
fn matches(record: &Map<String, Value>, partial: &Map<String, Value>) -> bool {
    partial
        .iter()
        .all(|(k, v)| record.get(k).is_some_and(|have| have == v))
}

/// Remove every record that matches any of the partial records. A `partials`
/// that is not an array removes nothing.
pub fn reject_where(records: &Value, partials: &Value) -> Vec<Value> {
    let records: Vec<Value> = match records {
        Value::Array(items) => items.clone(),
        _ => return Vec::new(),
    };
    let Some(partials) = partials.as_array() else {
        return records;
    };
    let partials: Vec<&Map<String, Value>> = partials.iter().filter_map(Value::as_object).collect();
    records
        .into_iter()
        .filter(|record| match record.as_object() {
            Some(r) => !partials.iter().any(|p| matches(r, p)),
            None => true,
        })
        .collect()
}

/// Merge partial records into the records they are compatible with.
///
/// For each partial, in order: it is compatible with a record when none of its
/// `protected` keys disagree with that record. Its other keys are then added to
/// every compatible record (overwriting anything an earlier partial added,
/// never a protected key). A partial compatible with no record is appended as a
/// new record. Only the original records are candidates — a record an earlier
/// partial appended is never extended by a later one.
///
/// This is the shape of GitHub's `include` and of Buildkite's `adjustments`,
/// with the format's rule about which keys are protected supplied by the
/// caller.
pub fn extend_where(records: &Value, partials: &Value, protected: &Value) -> Vec<Value> {
    let mut records: Vec<Map<String, Value>> = match records {
        Value::Array(items) => items.iter().filter_map(Value::as_object).cloned().collect(),
        _ => Vec::new(),
    };
    let Some(partials) = partials.as_array() else {
        return records.into_iter().map(Value::Object).collect();
    };
    let protected: Vec<&str> = match protected {
        Value::Array(keys) => keys.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };

    let mut appended: Vec<Map<String, Value>> = Vec::new();
    for partial in partials.iter().filter_map(Value::as_object) {
        let mut applied = false;
        for record in records.iter_mut() {
            let conflicts = partial.iter().any(|(k, v)| {
                protected.contains(&k.as_str()) && record.get(k).is_some_and(|have| have != v)
            });
            if conflicts {
                continue;
            }
            for (k, v) in partial {
                if !protected.contains(&k.as_str()) {
                    record.insert(k.clone(), v.clone());
                }
            }
            applied = true;
        }
        if !applied {
            appended.push(partial.clone());
        }
    }
    records
        .into_iter()
        .chain(appended)
        .map(Value::Object)
        .collect()
}

/// The object without the named keys. Not an object: unchanged.
pub fn omit(object: &Value, keys: &Value) -> Value {
    let (Some(map), Some(keys)) = (object.as_object(), keys.as_array()) else {
        return object.clone();
    };
    let drop: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
    Value::Object(
        map.iter()
            .filter(|(k, _)| !drop.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// An object's keys, in order. Not an object: empty.
pub fn keys(object: &Value) -> Vec<Value> {
    object
        .as_object()
        .map(|m| m.keys().map(|k| Value::String(k.clone())).collect())
        .unwrap_or_default()
}
