//! Typed module settings, stored as one JSON object under the module's `settings` key.
//!
//! A module describes its settings as a serde struct with `#[serde(default)]` and a
//! `Default`. Reading merges the stored object over the defaults, so settings added in a
//! later version simply appear with their default. Changing them goes through [`patch`]:
//! every key of the patch must exist (a typo is an error, not a silent no-op), the result
//! must deserialize, and the module's own check must pass before anything is stored.

use proto::ApiError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::dlog;
use crate::module::ModuleCtx;

const KEY: &str = "settings";

/// The module's settings, defaults filled in. A stored object that no longer fits the type
/// (for example written by a newer build) is logged and replaced by the defaults.
pub fn load<T: DeserializeOwned + Serialize + Default>(ctx: &ModuleCtx) -> Result<T, ApiError> {
    let Some(stored) = ctx.settings().get(KEY)? else {
        return Ok(T::default());
    };
    let merged = merge(to_value(&T::default())?, stored);
    Ok(serde_json::from_value(merged).unwrap_or_else(|e| {
        dlog!(
            "WARN",
            "{}: stored settings unreadable ({e}), using defaults",
            ctx.name()
        );
        T::default()
    }))
}

/// Applies `patch` (a partial object) to the stored settings. `null` resets a key to its
/// default, or removes an entry of an open map. `open_maps` names the keys whose objects
/// take new entries (for example `presets`); a new entry is checked against the map's first
/// existing one. `check` gets the result and may refuse it with a message.
pub fn patch<T: DeserializeOwned + Serialize + Default>(
    ctx: &ModuleCtx,
    patch: &Value,
    open_maps: &[&str],
    check: impl FnOnce(&T) -> Result<(), String>,
) -> Result<T, ApiError> {
    if !patch.is_object() {
        return Err(ApiError::invalid_args("settings must be a JSON object"));
    }
    let current = to_value(&load::<T>(ctx)?)?;
    check_keys(patch, &current, "", open_maps)?;
    let merged = merge(current, patch.clone());
    let next: T =
        serde_json::from_value(merged).map_err(|e| ApiError::invalid_args(format!("settings: {e}")))?;
    check(&next).map_err(|m| ApiError::invalid_args(format!("settings: {m}")))?;
    ctx.settings().set(KEY, &to_value(&next)?)?;
    Ok(next)
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::internal(format!("settings: {e}")))
}

/// Deep merge: objects merge key by key, anything else replaces; `null` removes the key.
pub fn merge(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut b), Value::Object(p)) => {
            // Adjacently tagged enums replace the old variant, including its old fields.
            if p.get("kind").is_some() && b.get("kind").is_some() && p.get("kind") != b.get("kind") {
                return Value::Object(p);
            }
            for (k, v) in p {
                match v {
                    Value::Null => {
                        b.remove(&k);
                    }
                    v => {
                        let merged = match b.remove(&k) {
                            Some(old) => merge(old, v),
                            None => v,
                        };
                        b.insert(k, merged);
                    }
                }
            }
            Value::Object(b)
        }
        (_, p) => p,
    }
}

fn check_keys(patch: &Value, reference: &Value, path: &str, open_maps: &[&str]) -> Result<(), ApiError> {
    let (Some(p), Some(r)) = (patch.as_object(), reference.as_object()) else {
        return Ok(());
    };
    if p.get("kind").is_some() && r.get("kind").is_some() && p.get("kind") != r.get("kind") {
        return Ok(()); // The typed enum deserializer validates the new variant and its fields.
    }
    let open = open_maps.contains(&path);
    let template: Option<&Value> = open.then(|| r.values().next()).flatten();
    for (k, v) in p {
        let at = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        match r.get(k).or(template) {
            Some(sub) => check_keys(v, sub, &at, open_maps)?,
            None if open => {}
            None => {
                return Err(ApiError::invalid_args(format!(
                    "unknown setting '{at}' (known: {})",
                    known(r)
                )));
            }
        }
    }
    Ok(())
}

fn known(r: &Map<String, Value>) -> String {
    r.keys().cloned().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_is_deep_and_null_removes() {
        let base = json!({"a": 1, "b": {"x": 1, "y": 2}, "m": {"p": {"q": 1}}});
        let out = merge(base, json!({"b": {"y": 3}, "a": null, "m": {"n": {"q": 2}}}));
        assert_eq!(
            out,
            json!({"b": {"x": 1, "y": 3}, "m": {"p": {"q": 1}, "n": {"q": 2}}})
        );
    }

    #[test]
    fn unknown_keys_are_refused_except_in_open_maps() {
        let r = json!({"a": 1, "presets": {"custom": {"lock": true}}});
        assert!(check_keys(&json!({"a": 2}), &r, "", &[]).is_ok());
        assert!(
            check_keys(&json!({"b": 2}), &r, "", &[])
                .unwrap_err()
                .message
                .contains("'b'")
        );
        assert!(
            check_keys(
                &json!({"presets": {"night": {"lock": false}}}),
                &r,
                "",
                &["presets"]
            )
            .is_ok()
        );
        let e = check_keys(
            &json!({"presets": {"night": {"lokc": false}}}),
            &r,
            "",
            &["presets"],
        )
        .unwrap_err();
        assert!(e.message.contains("presets.night.lokc"), "{}", e.message);
        assert!(check_keys(&json!({"presets": {"night": {}}}), &r, "", &[]).is_err());
    }
}
