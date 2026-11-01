//! Project item filtering and ordering are shared by REST, native views and GraphQL.
use crate::backend::db::projects::{field_configuration, Field, Item, NewView, Refusal};
use anyhow::Result;
use serde_json::Value;

/// Split the small project query language without losing quoted field names or values.
pub fn query_terms(query: &str) -> Result<Vec<String>> {
    let mut terms = Vec::new();
    let mut term = String::new();
    anyhow::ensure!(query.len() <= 4096, Refusal::Invalid);
    let mut quoted = false;
    let mut escaped = false;
    for c in query.chars() {
        if escaped {
            if !matches!(c, '"' | '\\') {
                term.push('\\');
            }
            term.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !term.is_empty() {
                    terms.push(std::mem::take(&mut term));
                }
            }
            _ => term.push(c),
        }
    }
    if escaped {
        term.push('\\');
    }
    anyhow::ensure!(!quoted, Refusal::Invalid);
    if !term.is_empty() {
        terms.push(term);
    }
    Ok(terms)
}

/// Drop the terms that name `field`, so a saved view still matches items once that field is
/// deleted. Terms with spaces are quoted again so they parse back the same way.
pub fn without_field_terms(filter: &str, field: &str) -> Result<String> {
    if field.eq_ignore_ascii_case("is") {
        return Ok(filter.to_owned());
    }
    let kept: Vec<String> = query_terms(filter)?
        .into_iter()
        .filter(|term| {
            let term = term.strip_prefix('-').unwrap_or(term);
            term.split_once(':')
                .is_none_or(|(name, _)| !name.eq_ignore_ascii_case(field))
        })
        .map(|term| {
            if term
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '"' | '\\'))
            {
                format!("\"{}\"", term.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                term
            }
        })
        .collect();
    Ok(kept.join(" "))
}

pub fn filter_items(
    items: Vec<Item>,
    fields: &[Field],
    query: &str,
    view: Option<&NewView>,
) -> Result<Vec<Item>> {
    let mut terms = query_terms(query)?;
    if let Some(view) = view {
        terms.extend(query_terms(view.filter.as_deref().unwrap_or(""))?);
    }
    let mut result = Vec::new();
    for item in items {
        let mut matches = true;
        for term in &terms {
            let (negative, term) = term
                .strip_prefix('-')
                .map_or((false, term.as_str()), |s| (true, s));
            let matched = if let Some((name, value)) = term.split_once(':') {
                if name == "is" {
                    match value {
                        "issue" => item.issue_id.is_some(),
                        "pr" => item.pull_request_id.is_some(),
                        "draft" => item.repo_id.is_none(),
                        "open" => item.state == "open",
                        "closed" => item.state != "open",
                        _ => false,
                    }
                } else if let Some(field) =
                    fields.iter().find(|f| f.name.eq_ignore_ascii_case(name))
                {
                    display_value(field, &item.value(field)?)?.eq_ignore_ascii_case(value)
                } else {
                    false
                }
            } else {
                item.title.to_lowercase().contains(&term.to_lowercase())
            };
            if matched == negative {
                matches = false;
                break;
            }
        }
        if matches {
            result.push(item);
        }
    }
    if let Some(view) = view {
        // Decorate before sorting so malformed stored values cannot be swallowed in a comparator.
        let mut keyed = Vec::new();
        for item in result {
            let mut keys = Vec::new();
            for (id, direction) in &view.sort_by {
                let field = fields
                    .iter()
                    .find(|f| f.id == *id)
                    .ok_or(Refusal::Invalid)?;
                let value = item.value(field)?;
                keys.push((
                    value.as_number().cloned(),
                    display_value(field, &value)?,
                    direction == "desc",
                ));
            }
            keyed.push((item, keys));
        }
        keyed.sort_by(|(_, ak), (_, bk)| {
            for ((an, at, desc), (bn, bt, _)) in ak.iter().zip(bk) {
                let order = match (an, bn) {
                    (Some(a), Some(b)) => compare_numbers(a, b),
                    _ => at.to_lowercase().cmp(&bt.to_lowercase()),
                };
                let order = if *desc { order.reverse() } else { order };
                if !order.is_eq() {
                    return order;
                }
            }
            // The sort is stable, so ties keep the saved item order.
            std::cmp::Ordering::Equal
        });
        result = keyed.into_iter().map(|(item, _)| item).collect();
    }
    Ok(result)
}

fn compare_numbers(a: &serde_json::Number, b: &serde_json::Number) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let integer = |number: &serde_json::Number| {
        number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
    };
    let integer_float = |integer: i128, float: f64| {
        integer.cmp(&(float as i128)).then_with(|| {
            if float.fract() > 0.0 {
                Ordering::Less
            } else if float.fract() < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
    };
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(a), None) => integer_float(a, b.as_f64().unwrap()),
        (None, Some(b)) => integer_float(b, a.as_f64().unwrap()).reverse(),
        (None, None) => a.as_f64().unwrap().total_cmp(&b.as_f64().unwrap()),
    }
}

pub fn display_value(field: &Field, value: &Value) -> Result<String> {
    if value.is_null() {
        return Ok(String::new());
    }
    let config = field_configuration(field)?;
    if matches!(field.data_type.as_str(), "single_select" | "iteration") {
        let key = if field.data_type == "single_select" {
            "options"
        } else {
            "iterations"
        };
        if let Some(options) = config[key].as_array() {
            if let Some(option) = options.iter().find(|o| o["id"] == *value) {
                return Ok(option[if key == "options" { "name" } else { "title" }]
                    .as_str()
                    .unwrap_or("")
                    .to_owned());
            }
        }
        return Ok(String::new());
    }
    Ok(value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string()))
}
