use anyhow::Result;
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Not Found")]
    Missing,
    #[error("Project write denied")]
    Denied,
    #[error("Invalid project field")]
    Invalid,
}
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
