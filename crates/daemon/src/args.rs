//! Request arguments are checked against the command's [`ArgSpec`]s before the module sees
//! them, so modules can rely on the types.

use proto::ApiError;
use proto::catalog::ArgKind;
use serde_json::Value;

use crate::module::CommandSpec;

fn kind_matches(kind: ArgKind, v: &Value) -> bool {
    match kind {
        ArgKind::Int => v.is_i64(),
        ArgKind::Uint => v.is_u64(),
        ArgKind::Str => v.is_string(),
        ArgKind::Bool => v.is_boolean(),
        ArgKind::StrList => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
        ArgKind::Json | ArgKind::Unknown => true,
    }
}

fn kind_name(kind: ArgKind) -> &'static str {
    match kind {
        ArgKind::Int => "an integer",
        ArgKind::Uint => "a non-negative integer",
        ArgKind::Str => "a string",
        ArgKind::Bool => "true or false",
        ArgKind::StrList => "a list of strings",
        ArgKind::Json | ArgKind::Unknown => "any JSON value",
    }
}

/// Unknown arguments are rejected (rather than ignored) so a newer client never has an
/// option silently dropped by an older daemon. `null` counts as "not given".
pub fn validate(full_name: &str, spec: &CommandSpec, args: &Value) -> Result<(), ApiError> {
    let obj = args.as_object().ok_or_else(|| ApiError::invalid_args("arguments must be a JSON object"))?;
    for (key, value) in obj {
        let Some(arg) = spec.args.iter().find(|a| a.name == key) else {
            let known: Vec<&str> = spec.args.iter().map(|a| a.name).collect();
            return Err(ApiError::invalid_args(format!(
                "'{full_name}' has no argument '{key}' (known: {})",
                if known.is_empty() { "none".to_owned() } else { known.join(", ") }
            )));
        };
        if !value.is_null() && !kind_matches(arg.kind, value) {
            return Err(ApiError::invalid_args(format!("'{key}' must be {}", kind_name(arg.kind))));
        }
    }
    for arg in spec.args.iter().filter(|a| a.required) {
        if obj.get(arg.name).is_none_or(Value::is_null) {
            return Err(ApiError::invalid_args(format!("'{full_name}' needs argument '{}'", arg.name)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ArgSpec;
    use serde_json::json;

    static ARGS: [ArgSpec; 3] = [
        ArgSpec::new("task_id", ArgKind::Int, "").required(),
        ArgSpec::new("deps", ArgKind::StrList, ""),
        ArgSpec::new("force", ArgKind::Bool, ""),
    ];
    static CMD: CommandSpec = CommandSpec::new("renew", "").args(&ARGS);

    #[test]
    fn checks_names_types_and_required() {
        let ok = |v| validate("task.renew", &CMD, &v);
        assert!(ok(json!({"task_id": 1})).is_ok());
        assert!(ok(json!({"task_id": 1, "deps": ["wsl"], "force": true})).is_ok());
        assert!(ok(json!({"task_id": 1, "deps": null})).is_ok());
        assert!(ok(json!({})).unwrap_err().message.contains("needs argument 'task_id'"));
        assert!(ok(json!({"task_id": "1"})).unwrap_err().message.contains("integer"));
        assert!(ok(json!({"task_id": 1, "deps": [1]})).is_err());
        assert!(ok(json!({"task_id": 1, "bogus": 1})).unwrap_err().message.contains("no argument 'bogus'"));
    }
}
