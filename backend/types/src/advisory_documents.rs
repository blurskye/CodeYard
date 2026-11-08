use crate::advisories::Refusal;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
pub fn import_document(value: &Value) -> Result<Value> {
    validate_document(value, 100)
}

pub fn osv_document(value: &Value) -> Result<Value> {
    validate_document(value, crate::vulnerabilities::MAX_AFFECTED_PACKAGES)
}

fn validate_document(value: &Value, max_packages: usize) -> Result<Value> {
    ensure!(
        serde_json::to_vec(value)?.len() <= 1024 * 1024,
        Refusal::Invalid
    );
    let mut value = value.as_object().ok_or(Refusal::Invalid)?.clone();
    let allowed = [
        "ghsa_id",
        "cve_id",
        "url",
        "html_url",
        "repository_advisory_url",
        "summary",
        "description",
        "type",
        "severity",
        "source_code_location",
        "identifiers",
        "references",
        "published_at",
        "updated_at",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
        "vulnerabilities",
        "cvss",
        "cvss_severities",
        "epss",
        "cwes",
        "credits",
    ];
    ensure!(
        value.keys().all(|k| allowed.contains(&k.as_str())),
        Refusal::Invalid
    );
    let id = value
        .entry("ghsa_id")
        .or_insert_with(|| json!(crate::advisories::identifier()))
        .as_str()
        .ok_or(Refusal::Invalid)?;
    ensure!(
        id.len() == 19
            && id.starts_with("GHSA-")
            && id[5..].split('-').count() == 3
            && id[5..]
                .split('-')
                .all(|s| s.len() == 4 && s.bytes().all(|b| b"23456789cfghjmpqrvwx".contains(&b))),
        Refusal::Invalid
    );
    let now = json!(Utc::now());
    for key in ["published_at", "updated_at"] {
        value.entry(key).or_insert_with(|| now.clone());
    }
    for key in [
        "cve_id",
        "description",
        "repository_advisory_url",
        "source_code_location",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
        "credits",
        "cwes",
        "cvss",
    ] {
        value.entry(key).or_insert(Value::Null);
    }
    value.entry("type").or_insert(json!("reviewed"));
    value.entry("references").or_insert(json!([]));
    let id = value["ghsa_id"].as_str().unwrap().to_owned();
    value.insert(
        "url".into(),
        json!(format!("{}/advisories/{id}", crate::urls::api_url(""))),
    );
    value.insert(
        "html_url".into(),
        json!(format!(
            "{}/api/v3/advisories/{id}",
            crate::http_config::link_origin()
        )),
    );
    let mut identifiers = vec![json!({"type":"GHSA","value":id})];
    if let Some(cve) = value["cve_id"].as_str() {
        identifiers.push(json!({"type":"CVE","value":cve}));
    }
    value.insert("identifiers".into(), json!(identifiers));
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .ok_or(Refusal::Invalid)?;
    ensure!(
        !summary.trim().is_empty() && summary.len() <= 1024,
        Refusal::Invalid
    );
    ensure!(
        value["description"].is_null()
            || value["description"]
                .as_str()
                .is_some_and(|s| s.len() <= 65535),
        Refusal::Invalid
    );
    ensure!(
        value["cve_id"].is_null() || value["cve_id"].is_string(),
        Refusal::Invalid
    );
    ensure!(
        matches!(
            value.get("severity").and_then(Value::as_str),
            Some("low" | "medium" | "high" | "critical" | "unknown")
        ),
        Refusal::Invalid
    );
    ensure!(
        matches!(
            value["type"].as_str(),
            Some("reviewed" | "unreviewed" | "malware")
        ),
        Refusal::Invalid
    );
    for key in ["published_at", "updated_at"] {
        ensure!(value[key].as_str().is_some(), Refusal::Invalid);
    }
    for key in ["references", "cwes", "credits"] {
        ensure!(
            value[key].is_null() || value[key].as_array().is_some_and(|a| a.len() <= 100),
            Refusal::Invalid
        );
    }
    ensure!(
        value["references"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|v| v.as_str().is_some_and(|s| url::Url::parse(s).is_ok())),
        Refusal::Invalid
    );
    ensure!(
        value["cwes"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|v| v["cwe_id"].as_str().is_some() && v["name"].as_str().is_some()),
        Refusal::Invalid
    );
    // Imported credits and scoring extensions are not used as local account or scoring authority.
    value.insert("credits".into(), Value::Null);
    value.remove("cvss_severities");
    value.remove("epss");
    if !value["cvss"].is_null() {
        let vector = value["cvss"]["vector_string"]
            .as_str()
            .ok_or(Refusal::Invalid)?;
        let score = crate::advisories::cvss_score(vector)?;
        value.insert("cvss".into(), json!({"vector_string":vector,"score":score}));
    }
    for key in [
        "published_at",
        "updated_at",
        "github_reviewed_at",
        "nvd_published_at",
        "withdrawn_at",
    ] {
        ensure!(
            value[key].is_null()
                || value[key]
                    .as_str()
                    .is_some_and(|s| DateTime::parse_from_rfc3339(s).is_ok()),
            Refusal::Invalid
        );
    }
    let vulnerabilities = value
        .get("vulnerabilities")
        .and_then(Value::as_array)
        .ok_or(Refusal::Invalid)?;
    ensure!(vulnerabilities.len() <= max_packages, Refusal::Invalid);
    for v in vulnerabilities {
        ensure!(
            v["first_patched_version"].is_null() || v["first_patched_version"].is_string(),
            Refusal::Invalid
        );
        ensure!(
            v["vulnerable_functions"].is_null()
                || v["vulnerable_functions"]
                    .as_array()
                    .is_some_and(|a| a.len() <= 100 && a.iter().all(Value::is_string)),
            Refusal::Invalid
        );
        ensure!(
            v["package"]["name"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 255 && !s.contains('\0'))
                && v["package"]["ecosystem"]
                    .as_str()
                    .is_some_and(|s| !s.contains('\0'))
                && v["vulnerable_version_range"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 1024),
            Refusal::Invalid
        );
    }
    Ok(Value::Object(value))
}
