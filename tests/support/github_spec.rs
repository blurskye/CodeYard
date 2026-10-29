//! Checks `/api/v3` responses against GitHub's own REST description in `spec/github/`.
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, Mutex, OnceLock},
};

fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/spec/github/api.github.com.json.gz"
        );
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        let mut spec = serde_json::from_str(&text).unwrap();
        nullable(&mut spec);
        spec
    })
}

/// OpenAPI 3.0 marks nullable schemas with `nullable: true`; JSON Schema spells it with types.
/// GitHub's description also writes `oneOf` where its branches overlap (a validation error body
/// matches both of create-blob's 422 schemas), so it is read as `anyOf`.
fn nullable(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(nullable),
        Value::Object(map) => {
            map.values_mut().for_each(nullable);
            if let Some(branches) = map.remove("oneOf") {
                map.insert("anyOf".into(), branches);
            }
            if map.get("nullable") != Some(&Value::Bool(true)) {
                return;
            }
            map.remove("nullable");
            // Null must bypass every constraint, including enum and referenced alternatives.
            let inner = Value::Object(std::mem::take(map));
            map.insert("anyOf".into(), json!([inner, {"type": "null"}]));
        }
        _ => {}
    }
}

fn resolve(value: &Value) -> &Value {
    match value["$ref"].as_str() {
        Some(pointer) => resolve(
            spec()
                .pointer(pointer.trim_start_matches('#'))
                .unwrap_or_else(|| panic!("{pointer} is not in GitHub's description")),
        ),
        None => value,
    }
}

type Validators = HashMap<(String, u16), Arc<jsonschema::Validator>>;

fn validator(operation_id: &str, status: u16) -> Arc<jsonschema::Validator> {
    static VALIDATORS: OnceLock<Mutex<Validators>> = OnceLock::new();
    let key = (operation_id.to_owned(), status);
    let mut validators = VALIDATORS.get_or_init(Default::default).lock().unwrap();
    if let Some(validator) = validators.get(&key) {
        return validator.clone();
    }
    let operation = spec()["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|item| item.as_object().unwrap().values())
        .find(|operation| operation["operationId"] == operation_id)
        .unwrap_or_else(|| panic!("{operation_id} is not in GitHub's description"));
    let response = resolve(&operation["responses"][status.to_string()]);
    let mut schema = response["content"]["application/json"]["schema"].clone();
    // Some documented 422 responses omit their body schema but still use GitHub's validation error.
    if schema.is_null()
        && status == 422
        && (response.is_null() || response.get("description").is_some())
    {
        schema = json!({"$ref": "#/components/schemas/validation-error"});
    }
    if schema.is_null()
        && status == 503
        && (response.is_null() || response.get("content").is_none())
    {
        schema = resolve(&spec()["components"]["responses"]["service_unavailable"])["content"]
            ["application/json"]["schema"]
            .clone();
    }
    // Request parsing, authentication/scope failures, private-resource hiding and merge conflicts use the shared
    // REST error document. Some operations omit the response or describe it without a body schema;
    // never replace a documented schema or a documented non-JSON content type.
    if schema.is_null()
        && (response.is_null() || response.get("content").is_none())
        && matches!(status, 400 | 401 | 403 | 404 | 409)
    {
        schema = json!({"$ref": "#/components/schemas/basic-error"});
    }
    assert!(
        !schema.is_null(),
        "GitHub documents no JSON body for {operation_id} {status}"
    );
    let root = json!({"allOf": [schema], "components": spec()["components"]});
    let validator = Arc::new(
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft4)
            .build(&root)
            .unwrap_or_else(|error| panic!("{operation_id} {status}: {error}")),
    );
    validators.insert(key, validator.clone());
    validator
}

/// One line per failure, with the failures of each `oneOf`/`anyOf` branch listed beneath it, so
/// a mismatch names the field rather than only the branch set.
fn describe(error: &jsonschema::ValidationError, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let path = error.instance_path().to_string();
    let at = if path.is_empty() { "/" } else { path.as_str() };
    let branches = match error.kind() {
        jsonschema::error::ValidationErrorKind::AnyOf { context }
        | jsonschema::error::ValidationErrorKind::OneOfNotValid { context } => Some(context),
        _ => None,
    };
    match branches {
        Some(branches) => {
            lines.push(format!(
                "{indent}{at}: matches none of {} schemas",
                branches.len()
            ));
            for (index, errors) in branches.iter().enumerate() {
                lines.push(format!("{indent}  schema {index}:"));
                for error in errors {
                    describe(error, depth + 2, lines);
                }
            }
        }
        None => lines.push(format!("{indent}{at}: {error}")),
    }
}

/// Panics unless `body` is what GitHub documents for `operation_id` answering `status`.
pub fn assert_github_schema(operation_id: &str, status: u16, body: &Value) {
    let validator = validator(operation_id, status);
    let mut errors = Vec::new();
    for error in validator.iter_errors(body) {
        describe(&error, 0, &mut errors);
    }
    assert!(
        errors.is_empty(),
        "{operation_id} {status} differs from GitHub's schema:\n{}\n{body}",
        errors.join("\n")
    );
}
