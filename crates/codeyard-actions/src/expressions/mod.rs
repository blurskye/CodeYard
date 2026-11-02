//! Ported from actions/runner's `src/Sdk/Expressions` and `src/Sdk/DTExpressions2`.
//! The expression language shared by workflow planning and job execution.
use std::collections::BTreeMap;

mod eval;
mod hash_files;
mod parser;

/// A value in an expression. Object keys keep their case; lookups ignore it, as on GitHub.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(v) => Self::Bool(*v),
            serde_json::Value::Number(v) => Self::Number(v.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(v) => Self::String(v.clone()),
            serde_json::Value::Array(v) => Self::Array(v.iter().map(Self::from_json).collect()),
            serde_json::Value::Object(v) => Self::Object(
                v.iter()
                    .map(|(k, v)| (k.clone(), Self::from_json(v)))
                    .collect(),
            ),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(v) => (*v).into(),
            Self::Number(v) => serde_json::Number::from_f64(*v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Self::String(v) => v.clone().into(),
            Self::Array(v) => v.iter().map(Self::to_json).collect(),
            Self::Object(v) => {
                serde_json::Value::Object(v.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }

    /// GitHub's truthiness: false, 0, -0, NaN, "" and null are false.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(v) => *v,
            Self::Number(v) => *v != 0.0 && !v.is_nan(),
            Self::String(v) => !v.is_empty(),
            Self::Array(_) | Self::Object(_) => true,
        }
    }

    /// Text for interpolation, with JSON for arrays and objects.
    pub fn to_text(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(v) => v.to_string(),
            Self::Number(v) => number_text(*v),
            Self::String(v) => v.clone(),
            Self::Array(_) | Self::Object(_) => json_text(self, false, 0),
        }
    }
}

/// The named contexts an expression can read. Lookups ignore case.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contexts {
    pub values: BTreeMap<String, Value>,
}

/// The outcome so far, used by the status functions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub failed: bool,
    pub cancelled: bool,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("{0}")]
    Syntax(String),
    #[error("{0}")]
    Evaluation(String),
}

/// Evaluates one expression, without the `${{ }}` around it.
pub fn evaluate(expression: &str, contexts: &Contexts, status: Status) -> Result<Value, Error> {
    let tree = parser::parse(expression, contexts)?;
    eval::evaluate(&tree, contexts, status)
}

/// Evaluates an `if:` condition, adding the implicit success check when appropriate.
pub fn evaluate_condition(
    condition: &str,
    contexts: &Contexts,
    status: Status,
) -> Result<bool, Error> {
    let condition = condition.trim();
    let condition = if let Some(inner) = condition.strip_prefix("${{") {
        let end = expression_end(inner)?;
        if !inner[end + 2..].trim().is_empty() {
            return Err(Error::Syntax(
                "Unexpected symbol after condition expression".into(),
            ));
        }
        &inner[..end]
    } else {
        condition
    };
    if condition.trim().is_empty() {
        return Ok(!status.failed && !status.cancelled);
    }
    let tree = parser::parse(condition, contexts)?;
    if !tree.has_status_function() && (status.failed || status.cancelled) {
        return Ok(false);
    }
    Ok(eval::evaluate(&tree, contexts, status)?.is_truthy())
}

/// Replaces each `${{ }}` in a string. A whole expression keeps its value's type.
pub fn interpolate(template: &str, contexts: &Contexts, status: Status) -> Result<Value, Error> {
    let mut remaining = template;
    let mut output = String::new();
    while let Some(start) = remaining.find("${{") {
        output.push_str(&remaining[..start]);
        let inner = &remaining[start + 3..];
        let end = expression_end(inner)?;
        let value = evaluate(&inner[..end], contexts, status)?;
        remaining = &inner[end + 2..];
        if start == 0 && inner.len() + 3 == template.len() && remaining.is_empty() {
            return Ok(value);
        }
        output.push_str(&value.to_text());
    }
    output.push_str(remaining);
    Ok(Value::String(output))
}

fn expression_end(inner: &str) -> Result<usize, Error> {
    let bytes = inner.as_bytes();
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if quoted && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            quoted = !quoted;
        } else if !quoted && bytes[i] == b'}' && bytes.get(i + 1) == Some(&b'}') {
            return Ok(i);
        }
        i += 1;
    }
    Err(Error::Syntax("The expression is not closed. An unescaped ${{ sequence was found, but the closing }} sequence was not found.".into()))
}

fn lookup<'a, T>(map: &'a BTreeMap<String, T>, key: &str) -> Option<&'a T> {
    map.iter()
        .find(|(k, _)| fold(k) == fold(key))
        .map(|(_, v)| v)
}

// OrdinalIgnoreCase uses simple Unicode case folding, not culture-sensitive comparison.
fn fold(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            let first = upper.next().unwrap_or(c);
            if upper.next().is_some() {
                c
            } else {
                first
            }
        })
        .collect()
}

fn parse_number(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    if let Some(hex) = text.strip_prefix("0x") {
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return f64::NAN;
        }
        return u32::from_str_radix(hex, 16)
            .map(|v| v as i32 as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(octal) = text.strip_prefix("0o") {
        if octal.is_empty() || !octal.chars().all(|c| matches!(c, '0'..='7')) {
            return f64::NAN;
        }
        return u32::from_str_radix(octal, 8)
            .map(|v| v as i32 as f64)
            .unwrap_or(f64::NAN);
    }
    match text {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if text
            .chars()
            .any(|c| !c.is_ascii_digit() && !matches!(c, '+' | '-' | '.' | 'e' | 'E')) =>
        {
            f64::NAN
        }
        _ => text.parse().unwrap_or(f64::NAN),
    }
}

fn number_text(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    if value.is_nan() {
        return "NaN".into();
    }
    if value == f64::INFINITY {
        return "Infinity".into();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    // The runner uses invariant G15: fifteen significant digits and a padded exponent.
    let scientific = format!("{value:.14e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("formatted exponent");
    let exponent: i32 = exponent.parse().expect("formatted exponent number");
    if !(-4..15).contains(&exponent) {
        return format!(
            "{}E{}{:02}",
            mantissa.trim_end_matches('0').trim_end_matches('.'),
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        );
    }
    let decimals = (14 - exponent).max(0) as usize;
    let fixed = format!("{value:.decimals$}");
    if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.').into()
    } else {
        fixed
    }
}

#[cfg(test)]
mod tests;

fn json_text(value: &Value, pretty: bool, depth: usize) -> String {
    let (open, close, items) = match value {
        Value::Array(items) => (
            '[',
            ']',
            items
                .iter()
                .map(|v| json_text(v, pretty, depth + 1))
                .collect::<Vec<_>>(),
        ),
        Value::Object(items) => (
            '{',
            '}',
            items
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}{}",
                        serde_json::Value::String(k.clone()),
                        if pretty { " " } else { "" },
                        json_text(v, pretty, depth + 1)
                    )
                })
                .collect(),
        ),
        Value::String(v) => return serde_json::Value::String(v.clone()).to_string(),
        Value::Null => return "null".into(),
        _ => return value.to_text(),
    };
    if items.is_empty() {
        return format!("{open}{close}");
    }
    if pretty {
        format!(
            "{open}\n{}{}\n{}{close}",
            "  ".repeat(depth + 1),
            items.join(&format!(",\n{}", "  ".repeat(depth + 1))),
            "  ".repeat(depth)
        )
    } else {
        format!("{open}{}{close}", items.join(","))
    }
}
