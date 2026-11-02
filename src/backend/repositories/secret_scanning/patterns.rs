//! Custom pattern validation and matching use the same bounded Rust regex engine as built-ins.
use super::Detection;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy)]
pub enum Owner {
    Repository(i64),
    Organization(i64),
}
impl Owner {
    pub fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Repository(id) => (Some(id), None),
            Self::Organization(id) => (None, Some(id)),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Create {
    pub name: String,
    pub pattern: String,
    #[serde(default = "start_delimiter")]
    pub start_delimiter: String,
    #[serde(default = "end_delimiter")]
    pub end_delimiter: String,
    #[serde(default)]
    pub must_match: Vec<String>,
    #[serde(default)]
    pub must_not_match: Vec<String>,
}
fn start_delimiter() -> String {
    r"\A|[^0-9A-Za-z]".into()
}
fn end_delimiter() -> String {
    r"\z|[^0-9A-Za-z]".into()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Pattern {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub slug: String,
    pub state: String,
    pub push_protection_enabled: bool,
    pub start_delimiter: String,
    pub end_delimiter: String,
    pub must_match: Vec<String>,
    pub must_not_match: Vec<String>,
    pub custom_pattern_version: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct Update {
    pub custom_pattern_version: serde_json::Value,
    pub pattern: Option<String>,
    pub start_delimiter: Option<String>,
    pub end_delimiter: Option<String>,
    pub must_match: Option<Vec<String>>,
    pub must_not_match: Option<Vec<String>>,
}
#[derive(Deserialize)]
pub struct Delete {
    pub pattern_id: i64,
    pub custom_pattern_version: Option<String>,
}

struct Compiled {
    regex: Regex,
    positive: Vec<Regex>,
    negative: Vec<Regex>,
}
fn compile(
    pattern: &str,
    start: &str,
    end: &str,
    positive: &[String],
    negative: &[String],
) -> Result<Compiled> {
    ensure!(
        !pattern.is_empty()
            && pattern.len() <= 4096
            && start.len() <= 1024
            && end.len() <= 1024
            && positive.len() <= 10
            && negative.len() <= 10,
        "Invalid custom pattern"
    );
    // Each part must be a complete expression by itself. Otherwise a part can close the secret
    // group early, and a match can then succeed without the capture.
    for part in [pattern, start, end] {
        super::scanner::compile_regex(part)?;
    }
    let regex = super::scanner::compile_regex(&format!(
        "(?:{start})(?P<codeyard_secret>{pattern})(?:{end})"
    ))?;
    ensure!(
        !regex.is_match(""),
        "Custom pattern must not match empty text"
    );
    let compile_list = |list: &[String]| -> Result<Vec<Regex>> {
        list.iter()
            .map(|value| {
                ensure!(value.len() <= 4096, "Pattern is too long");
                super::scanner::compile_regex(value)
            })
            .collect()
    };
    Ok(Compiled {
        regex,
        positive: compile_list(positive)?,
        negative: compile_list(negative)?,
    })
}

impl Create {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 100
                && !self.name.chars().any(char::is_control),
            "Invalid custom pattern name"
        );
        compile(
            &self.pattern,
            &self.start_delimiter,
            &self.end_delimiter,
            &self.must_match,
            &self.must_not_match,
        )?;
        Ok(())
    }
    pub fn into_pattern(self, id: i64) -> Pattern {
        let now = Utc::now();
        Pattern {
            id,
            name: self.name,
            pattern: self.pattern,
            slug: format!("custom-pattern-{id}"),
            state: "unpublished".into(),
            push_protection_enabled: false,
            start_delimiter: self.start_delimiter,
            end_delimiter: self.end_delimiter,
            must_match: self.must_match,
            must_not_match: self.must_not_match,
            custom_pattern_version: Some(uuid::Uuid::new_v4().to_string()),
            created_at: now,
            updated_at: now,
        }
    }
}
impl Pattern {
    pub fn apply(&mut self, input: &Update) -> Result<()> {
        ensure!(
            input.pattern.is_some()
                || input.start_delimiter.is_some()
                || input.end_delimiter.is_some()
                || input.must_match.is_some()
                || input.must_not_match.is_some(),
            "No pattern changes supplied"
        );
        if let Some(value) = &input.pattern {
            self.pattern = value.clone();
        }
        if let Some(value) = &input.start_delimiter {
            self.start_delimiter = value.clone();
        }
        if let Some(value) = &input.end_delimiter {
            self.end_delimiter = value.clone();
        }
        if let Some(value) = &input.must_match {
            self.must_match = value.clone();
        }
        if let Some(value) = &input.must_not_match {
            self.must_not_match = value.clone();
        }
        compile(
            &self.pattern,
            &self.start_delimiter,
            &self.end_delimiter,
            &self.must_match,
            &self.must_not_match,
        )?;
        self.custom_pattern_version = Some(uuid::Uuid::new_v4().to_string());
        self.updated_at = Utc::now();
        Ok(())
    }
}

pub fn scan(patterns: &[Pattern], text: &str, push: bool) -> Result<Vec<Detection>> {
    let mut detections = Vec::new();
    for pattern in patterns.iter().filter(|pattern| {
        pattern.state == "published" && (!push || pattern.push_protection_enabled)
    }) {
        let compiled = compile(
            &pattern.pattern,
            &pattern.start_delimiter,
            &pattern.end_delimiter,
            &pattern.must_match,
            &pattern.must_not_match,
        )?;
        let mut from = 0;
        while let Some(captures) = compiled.regex.captures_at(text, from) {
            let secret = captures
                .name("codeyard_secret")
                .ok_or_else(|| anyhow::anyhow!("Custom pattern matched without a secret"))?;
            // Start at the capture end so neighboring secrets can share a delimiter.
            from = if secret.end() > from {
                secret.end()
            } else if let Some(character) = text[from..].chars().next() {
                from + character.len_utf8()
            } else {
                break;
            };
            if secret.is_empty()
                || !compiled
                    .positive
                    .iter()
                    .all(|regex| regex.is_match(secret.as_str()))
                || compiled
                    .negative
                    .iter()
                    .any(|regex| regex.is_match(secret.as_str()))
            {
                continue;
            }
            let (start_line, start_column) = super::scanner::position(text, secret.start());
            let last = secret.end()
                - secret
                    .as_str()
                    .chars()
                    .last()
                    .expect("nonempty secret")
                    .len_utf8();
            let (end_line, end_column) = super::scanner::position(text, last);
            detections.push(Detection {
                rule_id: format!("cp_{}", pattern.id),
                secret_type: pattern.slug.clone(),
                secret_type_display_name: pattern.name.clone(),
                secret: secret.as_str().into(),
                start_line,
                start_column,
                end_line,
                end_column,
            });
            ensure!(
                detections.len() <= super::scanner::MAX_DETECTIONS,
                "Blob exceeds secret scanning detection budget"
            );
        }
    }
    Ok(detections)
}
