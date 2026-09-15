//! Tool input schemas: the signatures `agt mcp tools` shows, the arguments a
//! call lacks, and the ones Streamable HTTP mirrors into headers.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// A tool as a one-line signature, such as `navigate(url: string, wait?: number)`.
pub(super) fn signature(name: &str, schema: &Value) -> String {
    format!("{name}({})", parameters(schema, 1).join(", "))
}

/// The required arguments `arguments` lacks.
pub(super) fn missing<'a>(schema: &'a Value, arguments: &Value) -> Vec<&'a str> {
    required(schema).into_iter().filter(|name| arguments.get(name).is_none()).collect()
}

/// The properties a call must give: those the schema requires, apart from
/// any with a default, which servers fill in although generated schemas often
/// require them too.
fn required(schema: &Value) -> Vec<&str> {
    let names = schema.get("required").and_then(Value::as_array);
    let defaulted = |name: &&str| schema["properties"][*name].get("default").is_some();
    names.into_iter().flatten().filter_map(Value::as_str).filter(|name| !defaulted(name)).collect()
}

fn parameters(schema: &Value, depth: usize) -> Vec<String> {
    let required = required(schema);
    let properties = schema.get("properties").and_then(Value::as_object);
    properties
        .into_iter()
        .flatten()
        .map(|(name, property)| {
            let optional = if required.contains(&name.as_str()) { "" } else { "?" };
            format!("{name}{optional}: {}", kind(property, depth))
        })
        .collect()
}

/// How `schema` reads as a type, with objects written out `depth` levels deep.
fn kind(schema: &Value, depth: usize) -> String {
    let join = |kinds: Vec<String>| kinds.join(" | ");
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return join(values.iter().map(Value::to_string).collect());
    }
    if let Some(value) = schema.get("const") {
        return value.to_string();
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array) {
            return join(options.iter().map(|option| kind(option, depth)).collect());
        }
    }
    match schema.get("type") {
        Some(Value::String(name)) => named(name, schema, depth),
        Some(Value::Array(names)) => join(
            names.iter().filter_map(Value::as_str).map(|name| named(name, schema, depth)).collect(),
        ),
        _ if schema.get("properties").is_some() => named("object", schema, depth),
        _ => "any".into(),
    }
}

fn named(name: &str, schema: &Value, depth: usize) -> String {
    match name {
        "array" => {
            let item = schema.get("items").map_or_else(|| "any".into(), |items| kind(items, depth));
            if item.contains(" | ") { format!("({item})[]") } else { format!("{item}[]") }
        }
        "object" if depth > 0 && schema.get("properties").is_some() => {
            format!("{{{}}}", parameters(schema, depth - 1).join(", "))
        }
        name => name.to_owned(),
    }
}

/// The arguments a schema mirrors into `Mcp-Param-` headers over Streamable
/// HTTP: each annotated property's path from the root and its header name,
/// or why the annotations are invalid, which leaves the tool out.
pub(super) fn header_params(schema: &Value) -> Result<Vec<(Vec<String>, String)>, String> {
    let mut found: Vec<(Vec<String>, String)> = Vec::new();
    collect(schema, &mut Vec::new(), true, &mut found)?;
    for (index, (_, name)) in found.iter().enumerate() {
        if found[..index].iter().any(|(_, other)| other.eq_ignore_ascii_case(name)) {
            return Err(format!("x-mcp-header {name} names two properties"));
        }
    }
    Ok(found)
}

/// Finds the annotations in `schema`, which `path` leads to from the root,
/// through `properties` alone while `reachable`.
fn collect(
    schema: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    found: &mut Vec<(Vec<String>, String)>,
) -> Result<(), String> {
    let object = match schema {
        Value::Array(items) => {
            return items.iter().try_for_each(|item| collect(item, path, false, found));
        }
        Value::Object(object) => object,
        _ => return Ok(()),
    };
    if let Some(name) = object.get("x-mcp-header") {
        let name = name
            .as_str()
            .filter(|name| is_token(name))
            .ok_or("an x-mcp-header is not a header name")?;
        if !reachable || path.is_empty() {
            return Err(format!(
                "x-mcp-header {name} is not on a property reached through properties alone"
            ));
        }
        if !matches!(
            object.get("type").and_then(Value::as_str),
            Some("string" | "integer" | "boolean")
        ) {
            return Err(format!("x-mcp-header {name} is not on a string, integer or boolean"));
        }
        found.push((path.clone(), name.to_owned()));
    }
    for (key, value) in object {
        match (key.as_str(), value) {
            ("x-mcp-header", _) => {}
            ("properties", Value::Object(properties)) => {
                for (property, schema) in properties {
                    path.push(property.clone());
                    let collected = collect(schema, path, reachable, found);
                    path.pop();
                    collected?;
                }
            }
            (_, value) => collect(value, path, false, found)?,
        }
    }
    Ok(())
}

/// Whether `name` is an HTTP field-name token (RFC 9110).
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// The `Mcp-Param-` headers `arguments` fill for `params`, leaving out
/// values that are absent, null or not a string, integer or boolean.
pub(super) fn param_headers(
    params: &[(Vec<String>, String)],
    arguments: &Value,
) -> Vec<(String, String)> {
    params
        .iter()
        .filter_map(|(path, name)| {
            let value = path.iter().try_fold(arguments, |value, key| value.get(key))?;
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
                _ => return None,
            };
            Some((format!("Mcp-Param-{name}"), header_value(&text)))
        })
        .collect()
}

/// `text` as a header value: as it is when it is plain visible ASCII, and
/// otherwise, or when it looks encoded already, Base64 between `=?base64?`
/// and `?=`.
pub(super) fn header_value(text: &str) -> String {
    let visible = text.bytes().all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte));
    let padded = text.starts_with([' ', '\t']) || text.ends_with([' ', '\t']);
    let sentinel = text.starts_with("=?base64?") && text.ends_with("?=");
    if visible && !padded && !sentinel {
        text.to_owned()
    } else {
        format!("=?base64?{}?=", STANDARD.encode(text))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schemas_read_as_signatures() {
        let schema = json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "button": { "type": "string", "enum": ["left", "right"] },
                "keys": { "type": "array", "items": { "type": "string" } },
                "modifiers": { "type": "array", "items": { "enum": ["Alt", "Shift"] } },
                "size": { "type": "object", "properties": { "width": { "type": "integer" }, "box": { "type": "object", "properties": { "x": {} } } }, "required": ["width"] },
                "timeout": { "type": ["number", "null"] },
                "target": { "anyOf": [{ "type": "string" }, { "const": 3 }] },
                "raw": {},
                "scale": { "enum": ["css", "device"], "default": "css" }
            },
            "required": ["url", "scale"]
        });
        assert_eq!(
            signature("click", &schema),
            "click(button?: \"left\" | \"right\", keys?: string[], modifiers?: (\"Alt\" | \"Shift\")[], raw?: any, scale?: \"css\" | \"device\", size?: {box?: object, width: integer}, target?: string | 3, timeout?: number | null, url: string)"
        );
        assert_eq!(signature("close", &json!({ "type": "object" })), "close()");
        assert_eq!(missing(&schema, &json!({ "button": "left" })), ["url"]);
        assert!(missing(&schema, &json!({ "url": "x" })).is_empty());
    }

    #[test]
    fn header_annotations_must_be_reachable_primitives() {
        let valid = json!({ "type": "object", "properties": {
            "region": { "type": "string", "x-mcp-header": "Region" },
            "nested": { "type": "object", "properties": { "id": { "type": "integer", "x-mcp-header": "Id" } } }
        }});
        let params = header_params(&valid).expect("valid");
        assert_eq!(
            params,
            [
                (vec!["nested".to_owned(), "id".to_owned()], "Id".to_owned()),
                (vec!["region".to_owned()], "Region".to_owned())
            ]
        );
        let arguments = json!({ "region": "us-west1", "nested": { "id": 42 } });
        assert_eq!(
            param_headers(&params, &arguments),
            [
                ("Mcp-Param-Id".to_owned(), "42".to_owned()),
                ("Mcp-Param-Region".to_owned(), "us-west1".to_owned())
            ]
        );
        assert!(
            param_headers(&params, &json!({ "region": null })).is_empty(),
            "null and absent values send nothing"
        );
        for (schema, problem) in [
            (
                json!({ "properties": { "a": { "type": "number", "x-mcp-header": "A" } } }),
                "not on a string, integer or boolean",
            ),
            (
                json!({ "properties": { "a": { "type": "array", "items": { "type": "string", "x-mcp-header": "A" } } } }),
                "not on a property reached through properties alone",
            ),
            (
                json!({ "properties": { "a": { "type": "string", "x-mcp-header": "Bad Name" } } }),
                "not a header name",
            ),
            (
                json!({ "properties": { "a": { "type": "string", "x-mcp-header": "A" }, "b": { "type": "string", "x-mcp-header": "a" } } }),
                "names two properties",
            ),
        ] {
            let error = header_params(&schema).err().unwrap_or_default();
            assert!(error.contains(problem), "{schema}: {error}");
        }
    }

    #[test]
    fn header_values_encode_what_headers_cannot_carry() {
        for (text, value) in [
            ("us-west1", "us-west1"),
            ("Hello, 世界", "=?base64?SGVsbG8sIOS4lueVjA==?="),
            (" padded ", "=?base64?IHBhZGRlZCA=?="),
            ("line1\nline2", "=?base64?bGluZTEKbGluZTI=?="),
            ("=?base64?literal?=", "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="),
        ] {
            assert_eq!(header_value(text), value, "{text:?}");
        }
    }
}
