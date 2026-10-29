use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

pub const MAX_BLOB_BYTES: usize = 1024 * 1024;
pub(super) const MAX_DETECTIONS: usize = 1000;
pub(super) const MAX_MATCHED_BYTES: usize = 4 * MAX_BLOB_BYTES;

pub(super) fn compile_regex(pattern: &str) -> Result<Regex> {
    // RE2 shorthand classes are ASCII. Rust's Unicode expansion can also exhaust the NFA budget.
    // Non-count braces are literals in RE2; preserve repetition counts and Unicode properties.
    let mut output = String::with_capacity(pattern.len());
    let (mut escaped, mut class, mut count, mut property) = (false, false, false, false);
    for (offset, character) in pattern.char_indices() {
        if escaped {
            let replacement = match character {
                'w' => Some("[0-9A-Za-z_]"),
                'W' => Some("[^0-9A-Za-z_]"),
                'd' => Some("[0-9]"),
                'D' => Some("[^0-9]"),
                's' => Some(r"[\t\n\f\r ]"),
                'S' => Some(r"[^\t\n\f\r ]"),
                'b' if !class => Some(r"(?-u:\b)"),
                'B' if !class => Some(r"(?-u:\B)"),
                _ => None,
            };
            if let Some(replacement) = replacement {
                let _ = output.pop();
                output.push_str(replacement);
            } else {
                output.push(character);
            }
            property = matches!(character, 'p' | 'P' | 'x' | 'u' | 'U');
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' => class = true,
            ']' => class = false,
            '{' if !class => {
                count = property
                    || pattern[offset + 1..]
                        .split_once('}')
                        .is_some_and(|(value, _)| {
                            let (minimum, maximum) = value
                                .split_once(',')
                                .map(|(min, max)| (min, Some(max)))
                                .unwrap_or((value, None));
                            !minimum.is_empty()
                                && minimum.bytes().all(|byte| byte.is_ascii_digit())
                                && maximum
                                    .is_none_or(|max| max.bytes().all(|byte| byte.is_ascii_digit()))
                        });
                if !count {
                    output.push('\\');
                }
            }
            '}' if !class => {
                if !count {
                    output.push('\\');
                }
                count = false;
            }
            _ => {}
        }
        property = false;
        output.push(character);
    }
    Ok(Regex::new(&output)?)
}

/// Values stay out of Debug output and logs. Columns are one-based byte positions.
#[derive(Clone, PartialEq)]
pub struct Detection {
    pub rule_id: String,
    pub secret_type: String,
    pub secret_type_display_name: String,
    pub secret: String,
    pub start_line: i64,
    pub end_line: i64,
    pub start_column: i64,
    pub end_column: i64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Allowlist {
    condition: String,
    regex_target: String,
    paths: Vec<String>,
    regexes: Vec<String>,
    stopwords: Vec<String>,
    commits: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rule {
    id: String,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    entropy: f64,
    secret_group: Option<usize>,
    #[serde(default)]
    allowlists: Vec<Allowlist>,
}

#[derive(Deserialize)]
struct Config {
    allowlist: Allowlist,
    rules: Vec<Rule>,
}

struct CompiledAllowlist {
    and: bool,
    target: String,
    paths: Vec<Regex>,
    regexes: Vec<Regex>,
    stopwords: Vec<String>,
    commits: Vec<String>,
}

impl CompiledAllowlist {
    fn new(input: Allowlist) -> Result<Self> {
        let compile = |patterns: Vec<String>| -> Result<Vec<Regex>> {
            patterns
                .into_iter()
                .map(|pattern| compile_regex(&pattern))
                .collect()
        };
        Ok(Self {
            and: input.condition.eq_ignore_ascii_case("AND"),
            target: input.regex_target,
            paths: compile(input.paths)?,
            regexes: compile(input.regexes)?,
            stopwords: input
                .stopwords
                .into_iter()
                .map(|word| word.to_ascii_lowercase())
                .collect(),
            commits: input.commits,
        })
    }

    fn allows(&self, path: &str, commit: &str, secret: &str, matched: &str, line: &str) -> bool {
        let target = match self.target.as_str() {
            "match" => matched,
            "line" => line,
            _ => secret,
        };
        let mut checks = Vec::new();
        if !self.paths.is_empty() {
            checks.push(self.paths.iter().any(|regex| regex.is_match(path)));
        }
        if !self.commits.is_empty() {
            checks.push(self.commits.iter().any(|sha| sha == commit));
        }
        if !self.regexes.is_empty() {
            checks.push(self.regexes.iter().any(|regex| regex.is_match(target)));
        }
        if !self.stopwords.is_empty() {
            let lower = secret.to_ascii_lowercase();
            checks.push(self.stopwords.iter().any(|word| lower.contains(word)));
        }
        !checks.is_empty()
            && if self.and {
                checks.into_iter().all(|yes| yes)
            } else {
                checks.into_iter().any(|yes| yes)
            }
    }
}

struct CompiledRule {
    input: Rule,
    regex: Option<Regex>,
    path: Option<Regex>,
    allowlists: Vec<CompiledAllowlist>,
}

struct Scanner {
    global: CompiledAllowlist,
    rules: Vec<CompiledRule>,
}

impl Scanner {
    fn load() -> Result<Self> {
        let mut config: Config =
            toml::from_str(include_str!("../../../../spec/gitleaks/gitleaks.toml"))?;
        // Personal tokens issued before prefixes were introduced remain valid, but cannot be
        // distinguished reliably from arbitrary hexadecimal values in source code.
        for (id, regex, keyword) in [
            (
                "codeyard-pat",
                r"(?-u:\b)cy_pat_[a-f0-9]{64}(?-u:\b)",
                "cy_pat_",
            ),
            (
                "codeyard-fine-grained-pat",
                r"(?-u:\b)cy_fgpat_[a-f0-9]{64}(?-u:\b)",
                "cy_fgpat_",
            ),
            (
                "codeyard-actions-token",
                r"(?-u:\b)ghs_[a-f0-9]{64}(?-u:\b)",
                "ghs_",
            ),
        ] {
            config.rules.insert(
                0,
                Rule {
                    id: id.into(),
                    regex: Some(regex.into()),
                    path: None,
                    keywords: vec![keyword.into()],
                    entropy: 0.0,
                    secret_group: None,
                    allowlists: vec![],
                },
            );
        }
        config
            .rules
            .sort_by_key(|rule| rule.id == "generic-api-key");
        let mut rules = Vec::new();
        for mut input in config.rules {
            let regex = input
                .regex
                .as_deref()
                .map(compile_regex)
                .transpose()
                .with_context(|| format!("invalid secret scanning rule {}", input.id))?;
            let path = input.path.as_deref().map(compile_regex).transpose()?;
            let allowlists = std::mem::take(&mut input.allowlists)
                .into_iter()
                .map(CompiledAllowlist::new)
                .collect::<Result<_>>()?;
            rules.push(CompiledRule {
                input,
                regex,
                path,
                allowlists,
            });
        }
        Ok(Self {
            global: CompiledAllowlist::new(config.allowlist)?,
            rules,
        })
    }

    fn scan(&self, path: &str, commit: &str, text: &str) -> Result<Vec<Detection>> {
        if self.global.allows(path, commit, "", "", "") {
            return Ok(vec![]);
        }
        let lower = text.to_ascii_lowercase();
        let mut matched_bytes = 0usize;
        let mut found = Vec::new();
        let mut spans = Vec::new();
        for rule in &self.rules {
            if rule
                .path
                .as_ref()
                .is_some_and(|regex| !regex.is_match(path))
            {
                continue;
            }
            if !rule.input.keywords.is_empty()
                && !rule.input.keywords.iter().any(|word| lower.contains(word))
            {
                continue;
            }
            let Some(regex) = &rule.regex else { continue };
            let mut from = 0;
            while let Some(captures) = regex.captures_at(text, from) {
                let matched = captures.get(0).expect("whole match");
                let secret = match rule.input.secret_group {
                    Some(group) => captures.get(group),
                    None => (1..captures.len())
                        .find_map(|group| captures.get(group))
                        .or(Some(matched)),
                };
                let end = secret.map_or(matched.end(), |secret| secret.end());
                from = if end > from {
                    end
                } else if let Some(character) = text[from..].chars().next() {
                    from + character.len_utf8()
                } else {
                    break;
                };
                let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
                    continue;
                };
                if entropy(secret.as_str()) < rule.input.entropy {
                    continue;
                }
                let line_start = text[..matched.start()]
                    .rfind('\n')
                    .map_or(0, |offset| offset + 1);
                let line_end = text[matched.end()..]
                    .find('\n')
                    .map_or(text.len(), |offset| matched.end() + offset);
                let line = &text[line_start..line_end];
                if self
                    .global
                    .allows(path, commit, secret.as_str(), matched.as_str(), line)
                    || rule.allowlists.iter().any(|allowlist| {
                        allowlist.allows(path, commit, secret.as_str(), matched.as_str(), line)
                    })
                {
                    continue;
                }
                // A more specific provider rule owns a value already found by another rule.
                if spans
                    .iter()
                    .any(|(start, end)| secret.start() >= *start && secret.end() <= *end)
                {
                    continue;
                }
                let (start_line, start_column) = position(text, secret.start());
                let last = secret.end() - 1;
                let (end_line, end_column) = position(text, last);
                let (secret_type, display) = identity(&rule.input.id);
                matched_bytes = matched_bytes.saturating_add(secret.len());
                ensure!(matched_bytes <= MAX_MATCHED_BYTES && found.len() < MAX_DETECTIONS, "Blob exceeds secret scanning detection budget");
                spans.push((secret.start(), secret.end()));
                found.push(Detection {
                    rule_id: rule.input.id.clone(),
                    secret_type,
                    secret_type_display_name: display,
                    secret: secret.as_str().into(),
                    start_line,
                    end_line,
                    start_column,
                    end_column,
                });
                ensure!(
                    found.len() <= MAX_DETECTIONS,
                    "Blob exceeds secret scanning detection budget"
                );
            }
        }
        Ok(found)
    }
}

pub(super) fn position(text: &str, offset: usize) -> (i64, i64) {
    let prefix = &text.as_bytes()[..offset];
    let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = prefix.iter().rposition(|byte| *byte == b'\n').map_or(offset + 1, |last| offset - last);
    (line as i64, column as i64)
}

fn entropy(secret: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in secret.bytes() {
        counts[byte as usize] += 1;
    }
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let frequency = count as f64 / secret.len() as f64;
            -frequency * frequency.log2()
        })
        .sum()
}

fn identity(id: &str) -> (String, String) {
    let (kind, display) = match id {
        "github-pat" => (
            "github_personal_access_token",
            "GitHub Personal Access Token",
        ),
        "github-fine-grained-pat" => (
            "github_personal_access_token_v2",
            "GitHub Fine-Grained Personal Access Token",
        ),
        "github-app-token" => (
            "github_installation_access_token",
            "GitHub App Installation Access Token",
        ),
        "github-oauth" => ("github_oauth_access_token", "GitHub OAuth Access Token"),
        "github-refresh-token" => ("github_refresh_token", "GitHub Refresh Token"),
        "aws-access-token" => ("aws_access_key_id", "AWS Access Key ID"),
        "codeyard-pat" => (
            "codeyard_personal_access_token",
            "Codeyard Personal Access Token",
        ),
        "codeyard-fine-grained-pat" => (
            "codeyard_fine_grained_personal_access_token",
            "Codeyard Fine-grained Personal Access Token",
        ),
        "codeyard-actions-token" => ("codeyard_actions_token", "Codeyard Actions Token"),
        "slack-bot-token" => ("slack_bot_token", "Slack Bot Token"),
        "slack-user-token" => ("slack_user_token", "Slack User Token"),
        "slack-webhook-url" => ("slack_incoming_webhook_url", "Slack Incoming Webhook URL"),
        "gcp-api-key" => ("google_api_key", "Google API Key"),
        "gitlab-pat" => (
            "gitlab_personal_access_token",
            "GitLab Personal Access Token",
        ),
        "npm-access-token" => ("npm_access_token", "npm Access Token"),
        "pypi-upload-token" => ("pypi_upload_token", "PyPI Upload Token"),
        "sendgrid-api-token" => ("sendgrid_api_key", "SendGrid API Key"),
        "twilio-api-key" => ("twilio_api_key", "Twilio API Key"),
        "stripe-access-token" => ("stripe_api_key", "Stripe API Key"),
        "openai-api-key" => ("openai_api_key", "OpenAI API Key"),
        "anthropic-api-key" => ("anthropic_api_key", "Anthropic API Key"),
        "generic-api-key" => ("generic_api_key", "Generic API Key"),
        "private-key" => ("private_key", "Private Key"),
        _ => {
            return (
                id.replace('-', "_"),
                id.split('-')
                    .map(|word| {
                        let mut chars = word.chars();
                        chars
                            .next()
                            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
    };
    (kind.into(), display.into())
}

fn configured() -> Result<&'static Scanner> {
    static SCANNER: OnceLock<std::result::Result<Scanner, String>> = OnceLock::new();
    SCANNER
        .get_or_init(|| Scanner::load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))
}

pub fn secret_types() -> Result<Vec<(String, String)>> {
    let mut types = configured()?
        .rules
        .iter()
        .filter(|rule| rule.regex.is_some())
        .map(|rule| identity(&rule.input.id))
        .collect::<Vec<_>>();
    types.sort();
    types.dedup();
    Ok(types)
}

/// Scans UTF-8 text blobs only. Binary and oversized blobs are deliberately skipped.
pub fn scan(path: &str, commit: &str, bytes: &[u8]) -> Result<Vec<Detection>> {
    let scanner = configured()?;
    if bytes.len() > MAX_BLOB_BYTES || bytes.contains(&0) {
        return Ok(vec![]);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(vec![]);
    };
    scanner.scan(path, commit, text)
}
