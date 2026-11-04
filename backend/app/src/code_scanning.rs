//! Open scanners submit SARIF; queued uploads survive restarts and commit atomically.
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_db_core::DbPool;
use codeyard_db_security::code_scanning::Finding;
use codeyard_db_security::code_scanning::Run;
use codeyard_db_security::code_scanning::Upload;
use codeyard_webhooks::payloads;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::Read;
#[cfg(test)]
mod tests;

// Together with the 1 KiB category, references must fit PostgreSQL's B-tree key budget.
pub const MAX_REFERENCE_BYTES: usize = 1024;
pub const MAX_COMPRESSED: usize = 10 * 1024 * 1024;
const MAX_EXPANDED: usize = 100 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifRun {
    tool: Tool,
    #[serde(default)]
    results: Vec<SarifResult>,
    #[serde(default)]
    automation_details: Value,
    #[serde(default)]
    artifacts: Vec<Value>,
    #[serde(default)]
    original_uri_base_ids: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
struct Tool {
    driver: Driver,
    #[serde(default)]
    extensions: Vec<Driver>,
}
#[derive(Deserialize)]
struct Driver {
    name: String,
    version: Option<String>,
    #[serde(rename = "semanticVersion")]
    semantic_version: Option<String>,
    guid: Option<String>,
    #[serde(default)]
    rules: Vec<Value>,
    #[serde(default, rename = "globalMessageStrings")]
    global_messages: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule: Option<Value>,
    rule_id: Option<String>,
    rule_index: Option<i64>,
    kind: Option<String>,
    level: Option<String>,
    message: Value,
    #[serde(default)]
    locations: Vec<Value>,
    #[serde(default)]
    partial_fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    code_flows: Vec<Value>,
}

fn text(value: &Value) -> String {
    value
        .get("text")
        .or_else(|| value.get("markdown"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn no_nul(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.contains('\0'),
        Value::Array(values) => values.iter().all(no_nul),
        Value::Object(values) => values
            .iter()
            .all(|(key, value)| !key.contains('\0') && no_nul(value)),
        _ => true,
    }
}

fn result_message(message: &Value, rule: Option<&Value>, component: &Driver) -> Result<String> {
    let template = if message["text"].is_string() || message["markdown"].is_string() {
        text(message)
    } else {
        let id = message["id"].as_str().context("Missing result message")?;
        let value = rule
            .and_then(|rule| rule["messageStrings"].get(id))
            .or_else(|| component.global_messages.get(id))
            .context("Unknown result message ID")?;
        text(value)
    };
    let mut output = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                output.push('{');
            } else {
                let mut index = String::new();
                loop {
                    let ch = chars.next().context("Invalid message placeholder")?;
                    if ch == '}' {
                        break;
                    }
                    ensure!(
                        ch.is_ascii_digit() && index.len() < 8,
                        "Invalid message placeholder"
                    );
                    index.push(ch);
                }
                let index: usize = index.parse().context("Invalid message argument index")?;
                let argument = message["arguments"]
                    .get(index)
                    .and_then(Value::as_str)
                    .context("Missing message argument")?;
                output.push_str(argument);
            }
        } else if ch == '}' && chars.peek() == Some(&'}') {
            chars.next();
            output.push('}');
        } else {
            output.push(ch);
        }
        ensure!(output.len() <= 1024 * 1024, "Result message exceeds 1 MiB");
    }
    Ok(output)
}

/// Measure JSON before allocating Value vectors or maps. Each node pays for its container
/// entry as well as its text, including metadata that the projection will not use.
fn measure_json(bytes: &[u8]) -> Result<()> {
    use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess};
    use std::cell::Cell;
    #[derive(Clone, Copy)]
    struct Measure<'a>(&'a Cell<usize>);
    impl Measure<'_> {
        fn add<E: de::Error>(self, bytes: usize) -> std::result::Result<(), E> {
            let used = self.0.get().saturating_add(bytes).saturating_add(64);
            self.0.set(used);
            if used > 64 * 1024 * 1024 {
                return Err(E::custom("SARIF allocation budget exceeded"));
            }
            Ok(())
        }
    }
    impl<'de> DeserializeSeed<'de> for Measure<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> std::result::Result<(), D::Error> {
            deserializer.deserialize_any(self)
        }
    }
    impl<'de> de::Visitor<'de> for Measure<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded SARIF JSON")
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
            self.add(0)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
            self.add(0)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
            self.add(0)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
            self.add(0)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
            self.add(0)
        }
        fn visit_str<E: de::Error>(self, text: &str) -> std::result::Result<(), E> {
            self.add(text.len())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
            self.add(0)?;
            while seq.next_element_seed(self)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
            self.add(0)?;
            while map.next_key_seed(self)?.is_some() {
                map.next_value_seed(self)?;
            }
            Ok(())
        }
    }
    let used = Cell::new(0);
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    Measure(&used).deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(())
}

fn artifact_path(run: &SarifRun, physical: &Value, checkout: Option<&str>) -> Result<String> {
    let artifact = &physical["artifactLocation"];
    let indexed = artifact["index"]
        .as_u64()
        .and_then(|i| run.artifacts.get(i as usize));
    let location = if artifact["uri"].is_string() {
        artifact
    } else {
        &indexed.context("Missing artifact location")?["location"]
    };
    let uri = location["uri"].as_str().context("Missing artifact URI")?;
    let base_id = location["uriBaseId"].as_str();
    ensure!(
        uri.len() <= 16 * 1024 && base_id.is_none_or(|id| id.len() <= 1024),
        "Artifact URI metadata exceeds its limit"
    );
    let mut parts = vec![uri];
    let mut next = base_id;
    let mut seen = HashSet::new();
    while let Some(id) = next {
        ensure!(
            seen.len() < 32 && seen.insert(id),
            "Cyclic or excessive artifact URI bases"
        );
        let base = run
            .original_uri_base_ids
            .get(id)
            .context("Unknown artifact URI base")?;
        let uri = base["uri"].as_str().context("Missing artifact base URI")?;
        ensure!(uri.len() <= 16 * 1024, "Artifact base URI exceeds 16 KiB");
        parts.push(uri);
        next = base["uriBaseId"].as_str();
    }
    let fallback = "https://codeyard.invalid/";
    // The checkout names a directory, also without a trailing slash.
    let checkout = checkout.map(|base| {
        if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        }
    });
    let mut absolute = url::Url::parse(checkout.as_deref().unwrap_or(fallback))?;
    for part in parts.into_iter().rev() {
        absolute = absolute.join(part)?;
    }
    let absolute = absolute.to_string();
    let uri = absolute.strip_prefix(fallback).unwrap_or(&absolute);
    let uri = checkout
        .as_deref()
        .and_then(|base| {
            uri.strip_prefix(base.strip_suffix('/').unwrap_or(base))
                .and_then(|path| path.strip_prefix('/'))
        })
        .unwrap_or(uri);
    ensure!(
        !uri.starts_with('/') && !uri.contains("://") && !uri.contains('\\'),
        "Artifact is outside the checkout"
    );
    let path = codeyard_webui::decode_component(uri);
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.starts_with('/')
            && !path.split('/').any(|p| p == ".." || p == ".git")
            && !path.chars().any(char::is_control),
        "Invalid artifact path"
    );
    ensure!(!path.contains('\\'), "Invalid artifact path");
    Ok(path)
}

pub fn parse(upload: &Upload) -> Result<Vec<Run>> {
    ensure!(
        upload.ref_name.len() <= MAX_REFERENCE_BYTES,
        "Analysis reference exceeds 1 KiB"
    );
    ensure!(
        upload
            .checkout_uri
            .as_ref()
            .is_none_or(|uri| uri.len() <= 16 * 1024),
        "Checkout URI exceeds 16 KiB"
    );
    ensure!(
        upload.sarif.len() <= MAX_COMPRESSED.div_ceil(3) * 4,
        "SARIF exceeds 10 MiB compressed"
    );
    let compressed = STANDARD
        .decode(&upload.sarif)
        .context("Invalid base64 SARIF")?;
    ensure!(
        compressed.len() <= MAX_COMPRESSED,
        "SARIF exceeds 10 MiB compressed"
    );
    let mut bytes = Vec::new();
    flate2::read::MultiGzDecoder::new(&compressed[..])
        .take((MAX_EXPANDED + 1) as u64)
        .read_to_end(&mut bytes)
        .context("Invalid gzip SARIF")?;
    ensure!(
        bytes.len() <= MAX_EXPANDED,
        "SARIF exceeds 100 MiB uncompressed"
    );
    measure_json(&bytes)?;
    let mut raw: Value = serde_json::from_slice(&bytes).context("Invalid SARIF document")?;
    ensure!(raw["version"] == "2.1.0", "SARIF 2.1.0 is required");
    let raw_runs = raw["runs"].as_array_mut().context("Missing SARIF runs")?;
    ensure!(
        !raw_runs.is_empty() && raw_runs.len() <= 20,
        "SARIF requires 1 to 20 runs"
    );
    let mut keys = HashSet::new();
    let mut runs = Vec::new();
    // A small rule may be referenced by thousands of results. Bound the expanded projection too.
    let mut projected_bytes = 0_usize;
    for raw_run in raw_runs.drain(..) {
        let external = &raw_run["externalPropertyFileReferences"]["results"];
        ensure!(
            external.is_null() || external.as_array().is_some_and(Vec::is_empty),
            "External SARIF results must be inlined before upload"
        );
        ensure!(
            raw_run["invocations"]
                .as_array()
                .is_none_or(|invocations| invocations
                    .iter()
                    .all(|invocation| invocation["executionSuccessful"] != false)),
            "Scanner reported an unsuccessful invocation"
        );
        let saved_sarif = json!({"version":"2.1.0","$schema":"https://json.schemastore.org/sarif-2.1.0.json","runs":[&raw_run]}).to_string();
        let run: SarifRun = serde_json::from_value(raw_run)?;
        ensure!(!run.tool.driver.name.trim().is_empty(), "Missing tool name");
        ensure!(
            std::iter::once(&run.tool.driver)
                .chain(&run.tool.extensions)
                .all(|tool| !tool.name.contains('\0')
                    && [&tool.version, &tool.semantic_version, &tool.guid]
                        .iter()
                        .all(|value| value.as_ref().is_none_or(|value| !value.contains('\0')))),
            "Tool metadata contains a NUL character"
        );
        for tool in std::iter::once(&run.tool.driver).chain(&run.tool.extensions) {
            ensure!(
                tool.name.len() <= 1024
                    && [&tool.version, &tool.semantic_version, &tool.guid]
                        .iter()
                        .all(|value| value.as_ref().is_none_or(|value| value.len() <= 1024)),
                "Tool metadata exceeds 1 KiB per field"
            );
        }
        ensure!(
            run.results.len() <= 25_000
                && run.tool.driver.rules.len()
                    + run
                        .tool
                        .extensions
                        .iter()
                        .map(|extension| extension.rules.len())
                        .sum::<usize>()
                    <= 25_000,
            "SARIF permits 25000 results and rules per run"
        );
        ensure!(run.tool.extensions.len() <= 100, "Too many tool extensions");
        for rule in run
            .tool
            .driver
            .rules
            .iter()
            .chain(run.tool.extensions.iter().flat_map(|tool| &tool.rules))
        {
            ensure!(
                rule["properties"]["tags"]
                    .as_array()
                    .is_none_or(|tags| tags.len() <= 20 && tags.iter().all(Value::is_string)),
                "Rule tags must be at most 20 strings"
            );
        }
        let driver = &run.tool.driver;
        // Rule indexes are optional. Resolve IDs once instead of scanning every rule per result.
        let components = std::iter::once(driver)
            .chain(&run.tool.extensions)
            .map(|tool| {
                let mut rules = std::collections::HashMap::new();
                for rule in &tool.rules {
                    if let Some(id) = rule["id"].as_str() {
                        rules.entry(id).or_insert(rule);
                    }
                }
                (tool, rules)
            })
            .collect::<Vec<_>>();
        for (tool, _) in &components {
            if let Some(guid) = &tool.guid {
                uuid::Uuid::parse_str(guid).context("Invalid tool GUID")?;
            }
        }
        let driver_guid = driver
            .guid
            .as_deref()
            .map(uuid::Uuid::parse_str)
            .transpose()?
            .map(|guid| guid.to_string());
        let tool_key = driver_guid.as_deref().unwrap_or(&driver.name).to_owned();
        // SARIF separates the category from the run ID at the final slash.
        let category = run.automation_details["id"]
            .as_str()
            .and_then(|id| id.rsplit_once('/'))
            .map_or("", |(category, _)| category)
            .to_owned();
        ensure!(category.len() <= 1024, "Analysis category exceeds 1 KiB");
        // PostgreSQL text cannot hold NUL, and a failed write would hold up the upload queue.
        ensure!(
            !category.contains('\0'),
            "Analysis category contains a NUL character"
        );
        ensure!(
            keys.insert((tool_key.clone(), category.clone())),
            "Duplicate tool and category in upload"
        );
        let mut findings = Vec::new();
        let mut fingerprints = HashSet::new();
        for (source_index, result) in run.results.iter().enumerate() {
            let kind = result.kind.as_deref().unwrap_or("fail");
            ensure!(
                [
                    "fail",
                    "pass",
                    "notApplicable",
                    "open",
                    "review",
                    "informational"
                ]
                .contains(&kind),
                "Invalid result kind"
            );
            if matches!(kind, "pass" | "notApplicable") {
                continue;
            }
            ensure!(result.locations.len() <= 1000, "Too many result locations");
            ensure!(
                result
                    .code_flows
                    .iter()
                    .filter_map(|flow| flow["threadFlows"].as_array())
                    .flatten()
                    .filter_map(|thread| thread["locations"].as_array())
                    .map(Vec::len)
                    .sum::<usize>()
                    <= 10_000,
                "Too many thread flow locations"
            );
            // A result names its rule's tool component by extension index, GUID or name. The
            // driver is the default, and a reference that names nothing here is refused.
            let component_index = match result
                .rule
                .as_ref()
                .map(|rule| &rule["toolComponent"])
                .filter(|reference| reference.is_object())
            {
                None => 0,
                Some(reference) => if let Some(index) = reference["index"].as_u64() {
                    usize::try_from(index)
                        .ok()
                        .filter(|index| *index < run.tool.extensions.len())
                        .map(|index| index + 1)
                } else if let Some(guid) = reference["guid"].as_str() {
                    components.iter().position(|(tool, _)| {
                        tool.guid
                            .as_deref()
                            .is_some_and(|known| known.eq_ignore_ascii_case(guid))
                    })
                } else if let Some(name) = reference["name"].as_str() {
                    components.iter().position(|(tool, _)| tool.name == name)
                } else {
                    Some(0)
                }
                .context("Unknown result tool component")?,
            };
            let (component, rules) = &components[component_index];
            let rule_id = result
                .rule_id
                .as_deref()
                .or_else(|| result.rule.as_ref().and_then(|rule| rule["id"].as_str()));
            let rule_index = result
                .rule_index
                .or_else(|| result.rule.as_ref().and_then(|rule| rule["index"].as_i64()));
            ensure!(
                rule_index.is_none_or(|index| index >= -1),
                "Invalid rule index"
            );
            let rule_index = rule_index
                .filter(|index| *index >= 0)
                .map(|index| index as usize);
            let rule = rule_index
                .and_then(|i| component.rules.get(i))
                .or_else(|| rule_id.and_then(|id| rules.get(id).copied()));
            let id = rule_id
                .or_else(|| rule.and_then(|r| r["id"].as_str()))
                .context("Missing result rule")?;
            let location = result
                .locations
                .first()
                .context("Result has no usable location")?;
            let physical = &location["physicalLocation"];
            let path = artifact_path(&run, physical, upload.checkout_uri.as_deref())?;
            let region = &physical["region"];
            let start = region["startLine"].as_i64().unwrap_or(1);
            let end = region["endLine"].as_i64().unwrap_or(start);
            ensure!(start > 0 && end >= start, "Invalid SARIF region");
            let level = result
                .level
                .as_deref()
                .or_else(|| rule.and_then(|r| r["defaultConfiguration"]["level"].as_str()))
                .unwrap_or("warning");
            ensure!(
                ["none", "note", "warning", "error"].contains(&level),
                "Invalid result level"
            );
            let score = rule
                .and_then(|r| r["properties"]["security-severity"].as_str())
                .filter(|score| score.len() <= 64)
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|score| score.is_finite() && (0.0..=10.0).contains(score));
            let severity = score.map(|s| {
                if s >= 9.0 {
                    "critical"
                } else if s >= 7.0 {
                    "high"
                } else if s >= 4.0 {
                    "medium"
                } else {
                    "low"
                }
            });
            let message = result_message(&result.message, rule, component)?;
            let stable = if !result.fingerprints.is_empty() {
                serde_json::to_string(&result.fingerprints)?
            } else if !result.partial_fingerprints.is_empty() {
                serde_json::to_string(&result.partial_fingerprints)?
            } else {
                format!("{message}:{start}")
            };
            let component_key = (component_index != 0).then(|| {
                component
                    .guid
                    .as_deref()
                    .and_then(|guid| uuid::Uuid::parse_str(guid).ok())
                    .map(|guid| guid.to_string())
                    .unwrap_or_else(|| component.name.clone())
            });
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(component_key, id, &path, stable))?)
            );
            if !fingerprints.insert(fingerprint.clone()) {
                continue;
            }
            let rule = json!({"id":id,"name":rule.and_then(|r| r["name"].as_str()).unwrap_or(id),
                "severity":level,"security_severity_level":severity,
                "description":rule.map(|r| text(&r["shortDescription"])).unwrap_or_default(),
                "full_description":rule.map(|r| text(&r["fullDescription"])).unwrap_or_default(),
                "help":rule.map(|r| r["help"]["markdown"].as_str().map(str::to_owned).unwrap_or_else(||text(&r["help"]))),"help_uri":rule.and_then(|r| r["helpUri"].as_str()),
                "tags":rule.and_then(|r| r["properties"]["tags"].as_array()).map(|tags|tags.iter().take(10).collect::<Vec<_>>())});
            ensure!(no_nul(&rule), "Rule metadata contains a NUL character");
            let rule = rule.to_string();
            ensure!(
                rule.len() <= 1024 * 1024 && message.len() <= 1024 * 1024,
                "Result metadata exceeds 1 MiB"
            );
            ensure!(
                !message.contains('\0') && !path.contains('\0'),
                "Result text contains a NUL character"
            );
            projected_bytes = projected_bytes
                .saturating_add(rule.len() + message.len() + path.len() + fingerprint.len());
            ensure!(
                projected_bytes <= MAX_EXPANDED,
                "SARIF alert projection exceeds 100 MiB"
            );
            let priority = match severity {
                Some("critical") => 7,
                Some("high") => 6,
                Some("medium") => 5,
                Some(_) => 4,
                None => match level {
                    "error" => 3,
                    "warning" => 2,
                    "note" => 1,
                    _ => 0,
                },
            };
            let start_column = region["startColumn"].as_i64().unwrap_or(1);
            let end_column = region["endColumn"].as_i64();
            ensure!(
                start_column > 0
                    && end_column
                        .is_none_or(|column| column > 0 && (end > start || column >= start_column)),
                "Invalid SARIF columns"
            );
            let mut location =
                json!({"path":path,"start_line":start,"end_line":end,"start_column":start_column});
            if let Some(column) = end_column {
                location["end_column"] = json!(column);
            }
            findings.push(Finding {
                fingerprint,
                rule,
                message,
                priority,
                source_index: source_index as i64,
                location: location.to_string(),
            });
        }
        if findings.len() > 5000 {
            findings.sort_by_key(|finding| std::cmp::Reverse(finding.priority));
            findings.truncate(5000);
        }
        runs.push(Run { tool_key, tool:json!({"name":driver.name,"version":driver.semantic_version.as_ref().or(driver.version.as_ref()),"guid":driver_guid}).to_string(), category,
            rules_count:(driver.rules.len()+run.tool.extensions.iter().map(|tool|tool.rules.len()).sum::<usize>()) as i64, results_count:run.results.len() as i64, sarif:saved_sarif, findings });
    }
    Ok(runs)
}

pub async fn process_next(pool: &DbPool) -> Result<bool> {
    let Some(upload) = codeyard_db_security::code_scanning::pending(pool).await? else {
        return Ok(false);
    };
    let parsed = tokio::task::spawn_blocking({
        let upload = upload.clone();
        move || parse(&upload)
    })
    .await?;
    let changes = match parsed {
        Ok(runs) => {
            match codeyard_db_security::code_scanning::finish(pool, &upload, &runs, None).await {
                Ok(changes) => changes,
                Err(error)
                    if error
                        .downcast_ref::<sqlx::Error>()
                        .and_then(|error| error.as_database_error())
                        .and_then(|error| error.code())
                        .is_some_and(|code| {
                            code.starts_with("22") || code.starts_with("54") || code == "23514"
                        }) =>
                {
                    tracing::warn!(upload = %upload.id, "SARIF processing rejected by storage constraints");
                    codeyard_db_security::code_scanning::finish(
                        pool,
                        &upload,
                        &[],
                        Some("Analysis exceeds storage constraints"),
                    )
                    .await?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => {
            let message = format!("Invalid SARIF: {error}")
                .chars()
                .take(2048)
                .collect::<String>();
            codeyard_db_security::code_scanning::finish(pool, &upload, &[], Some(&message)).await?
        }
    };
    if let Some(actor) = upload.actor_id {
        for change in changes {
            if let Err(error) = notify(
                pool,
                actor,
                &change.alert,
                change.action,
                Some((&upload.ref_name, &upload.commit_sha)),
                change.instance.as_ref(),
            )
            .await
            {
                tracing::warn!(%error, alert_id=change.alert.id, "code scanning notification failed after commit");
            }
        }
    }
    Ok(true)
}

async fn notify(
    pool: &DbPool,
    actor: i64,
    alert: &codeyard_db_security::code_scanning::Alert,
    action: &'static str,
    revision: Option<(&str, &str)>,
    instance: Option<&codeyard_db_security::code_scanning::Instance>,
) -> Result<()> {
    let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, alert.repo_id).await?
    else {
        return Ok(());
    };
    let instance = match instance {
        Some(instance) => instance.clone(),
        None if revision.is_some() => return Ok(()),
        None => match codeyard_db_security::code_scanning::instance(pool, alert.id, None).await? {
            Some(instance) => instance,
            None => return Ok(()),
        },
    };
    let alert = alert.clone();
    let snapshot = repo.clone();
    let revision = revision.map(|(reference, commit)| (reference.to_owned(), commit.to_owned()));
    let dismissed = alert.dismissed_by;
    let mut people = vec![actor];
    people.extend(dismissed);
    crate::webhooks::emit(pool, &repo, "code_scanning_alert", people, move |owner, people| {
        let dismissed = dismissed.map(|id|codeyard_webhooks::payloads::person(&people.get(id)));
        let document = codeyard_webhooks::payloads::code_scanning::code_scanning_alert(&alert, std::slice::from_ref(&instance), owner, &snapshot.name, dismissed).expect("stored alert documents are valid JSON");
        let instance = &document["most_recent_instance"];
        let (reference,commit)=match &revision {Some((reference,commit))=>(json!(reference),json!(commit)),None=>(instance["ref"].clone(),instance["commit_sha"].clone())};
        json!({"action":action,"alert":document,"ref":reference,"commit_oid":commit,"repository":payloads::repository(&snapshot,owner),"sender":payloads::person(&people.get(actor))})
    }).await;
    Ok(())
}

pub async fn dismiss(
    pool: &DbPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    number: i64,
    input: &codeyard_db_security::code_scanning::Dismissal<'_>,
) -> Result<codeyard_db_security::code_scanning::Alert> {
    input.validate()?;
    let actor = actor.into();
    let actor_id = actor.user_id;
    let alert =
        codeyard_db_security::code_scanning::dismiss(pool, actor, repo, number, input).await?;
    if let Err(error) = notify(
        pool,
        actor_id,
        &alert,
        if input.state == "dismissed" {
            "closed_by_user"
        } else {
            "reopened_by_user"
        },
        None,
        None,
    )
    .await
    {
        tracing::warn!(%error, alert_id=alert.id, "code scanning notification failed after commit");
    }
    Ok(alert)
}

pub async fn run(pool: DbPool, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            break;
        }
        match process_next(&pool).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error,"could not process SARIF upload"),
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}
