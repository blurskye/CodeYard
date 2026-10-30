//! Open scanners submit SARIF; queued uploads survive restarts and commit atomically.
#[cfg(test)]
mod tests;
use crate::backend::db::{
    self,
    code_scanning::{Finding, Run, Upload},
    DbPool,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
};

pub const MAX_COMPRESSED: usize = 10 * 1024 * 1024;
const MAX_EXPANDED: usize = 100 * 1024 * 1024;

#[derive(Deserialize)]
struct Sarif {
    version: String,
    runs: Vec<SarifRun>,
}
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
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule: Option<Value>,
    rule_id: Option<String>,
    rule_index: Option<usize>,
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
    let base = base_id
        .and_then(|id| run.original_uri_base_ids.get(id))
        .and_then(|v| v["uri"].as_str());
    ensure!(
        base.is_none_or(|base| base.len() <= 16 * 1024),
        "Artifact base URI exceeds 16 KiB"
    );
    let absolute = base.map(|base| format!("{base}{uri}"));
    let uri = absolute.as_deref().unwrap_or(uri);
    let uri = checkout
        .and_then(|base| {
            uri.strip_prefix(base.strip_suffix('/').unwrap_or(base))
                .and_then(|path| path.strip_prefix('/'))
        })
        .unwrap_or(uri);
    ensure!(
        !uri.starts_with('/') && !uri.contains("://") && !uri.contains('\\'),
        "Artifact is outside the checkout"
    );
    let path = crate::webui::decode_component(uri);
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
    let sarif: Sarif = serde_json::from_slice(&bytes).context("Invalid SARIF document")?;
    ensure!(
        sarif.version == "2.1.0" && !sarif.runs.is_empty() && sarif.runs.len() <= 20,
        "SARIF 2.1.0 with 1 to 20 runs is required"
    );
    let raw: Value = serde_json::from_slice(&bytes)?;
    let mut keys = HashSet::new();
    let mut runs = Vec::new();
    // A small rule may be referenced by thousands of results. Bound the expanded projection too.
    let mut projected_bytes = 0_usize;
    for (index, run) in sarif.runs.iter().enumerate() {
        let external = &raw["runs"][index]["externalPropertyFileReferences"]["results"];
        ensure!(
            external.is_null() || external.as_array().is_some_and(Vec::is_empty),
            "External SARIF results must be inlined before upload"
        );
        ensure!(
            raw["runs"][index]["invocations"]
                .as_array()
                .is_none_or(|invocations| invocations
                    .iter()
                    .all(|invocation| invocation["executionSuccessful"] != false)),
            "Scanner reported an unsuccessful invocation"
        );
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
        let tool_key = driver.guid.as_deref().unwrap_or(&driver.name).to_owned();
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
            let rule_index = result.rule_index.or_else(|| {
                result
                    .rule
                    .as_ref()
                    .and_then(|rule| rule["index"].as_u64())
                    .map(|index| index as usize)
            });
            let rule = rule_index
                .and_then(|i| component.rules.get(i))
                .or_else(|| rule_id.and_then(|id| rules.get(id).copied()));
            let id = rule_id
                .or_else(|| rule.and_then(|r| r["id"].as_str()))
                .context("Missing result rule")?;
            let Some(location) = result.locations.first() else {
                continue;
            };
            let physical = &location["physicalLocation"];
            let path = artifact_path(run, physical, upload.checkout_uri.as_deref())?;
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
            let rule = json!({"id":id,"name":rule.and_then(|r| r["name"].as_str()).unwrap_or(id),
                "severity":level,"security_severity_level":severity,
                "description":rule.map(|r| text(&r["shortDescription"])).unwrap_or_default(),
                "full_description":rule.map(|r| text(&r["fullDescription"])).unwrap_or_default(),
                "help":rule.map(|r| r["help"]["markdown"].as_str().map(str::to_owned).unwrap_or_else(||text(&r["help"]))),"help_uri":rule.and_then(|r| r["helpUri"].as_str()),
                "tags":rule.and_then(|r| r["properties"]["tags"].as_array()).map(|tags|tags.iter().take(10).collect::<Vec<_>>())});
            let message = text(&result.message);
            let stable = result
                .partial_fingerprints
                .get("primaryLocationLineHash")
                .or_else(|| result.partial_fingerprints.values().next())
                .or_else(|| result.fingerprints.values().next())
                .cloned()
                .unwrap_or_else(|| format!("{message}:{start}"));
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(id, &path, stable))?)
            );
            if !fingerprints.insert(fingerprint.clone()) {
                continue;
            }
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
            findings.push(Finding { fingerprint, rule, message, priority, source_index:source_index as i64,
                location:json!({"path":path,"start_line":start,"end_line":end,"start_column":region["startColumn"].as_i64().unwrap_or(1),"end_column":region["endColumn"].as_i64().unwrap_or(1)}).to_string() });
        }
        if findings.len() > 5000 {
            findings.sort_by_key(|finding| std::cmp::Reverse(finding.priority));
            findings.truncate(5000);
        }
        runs.push(Run { tool_key, tool:json!({"name":driver.name,"version":driver.semantic_version.as_ref().or(driver.version.as_ref()),"guid":driver.guid}).to_string(), category,
            rules_count:(driver.rules.len()+run.tool.extensions.iter().map(|tool|tool.rules.len()).sum::<usize>()) as i64, results_count:run.results.len() as i64, sarif:json!({"version":"2.1.0","$schema":"https://json.schemastore.org/sarif-2.1.0.json","runs":[raw["runs"][index]]}).to_string(), findings });
    }
    Ok(runs)
}

pub async fn process_next(pool: &DbPool) -> Result<bool> {
    let Some(upload) = db::code_scanning::pending(pool).await? else {
        return Ok(false);
    };
    let parsed = tokio::task::spawn_blocking({
        let upload = upload.clone();
        move || parse(&upload)
    })
    .await?;
    let changes = match parsed {
        Ok(runs) => db::code_scanning::finish(pool, &upload, &runs, None).await?,
        Err(error) => {
            let message = format!("Invalid SARIF: {error}")
                .chars()
                .take(2048)
                .collect::<String>();
            db::code_scanning::finish(pool, &upload, &[], Some(&message)).await?
        }
    };
    if let Some(actor) = upload.actor_id {
        for change in changes {
            notify(
                pool,
                actor,
                &change.alert,
                change.action,
                Some((&upload.ref_name, &upload.commit_sha)),
            )
            .await?;
        }
    }
    Ok(true)
}

async fn notify(
    pool: &DbPool,
    actor: i64,
    alert: &db::code_scanning::Alert,
    action: &'static str,
    revision: Option<(&str, &str)>,
) -> Result<()> {
    use crate::backend::webhooks::{self, payloads};
    let Some(repo) = db::repositories::records::get_repository_by_id(pool, alert.repo_id).await?
    else {
        return Ok(());
    };
    let Some(instance) = db::code_scanning::instance(pool, alert.id, None).await? else {
        return Ok(());
    };
    let alert = alert.clone();
    let snapshot = repo.clone();
    let revision = revision.map(|(reference, commit)| (reference.to_owned(), commit.to_owned()));
    let dismissed = alert.dismissed_by;
    let mut people = vec![actor];
    people.extend(dismissed);
    webhooks::emit(pool, &repo, "code_scanning_alert", people, move |owner, people| {
        let dismissed = dismissed.map(|id|payloads::person(&people.get(id)));
        let document = payloads::code_scanning_alert(&alert, std::slice::from_ref(&instance), owner, &snapshot.name, dismissed).expect("stored alert documents are valid JSON");
        let instance = &document["most_recent_instance"];
        let (reference,commit)=match &revision {Some((reference,commit))=>(json!(reference),json!(commit)),None=>(instance["ref"].clone(),instance["commit_sha"].clone())};
        json!({"action":action,"alert":document,"ref":reference,"commit_oid":commit,"repository":payloads::repository(&snapshot,owner),"sender":payloads::person(&people.get(actor))})
    }).await;
    Ok(())
}

pub async fn dismiss(
    pool: &DbPool,
    actor: impl Into<db::users::tokens::Actor>,
    repo: i64,
    number: i64,
    input: &db::code_scanning::Dismissal<'_>,
) -> Result<db::code_scanning::Alert> {
    input.validate()?;
    let actor = actor.into();
    let alert = db::code_scanning::dismiss(pool, actor, repo, number, input).await?;
    notify(
        pool,
        actor.user_id,
        &alert,
        if input.state == "dismissed" {
            "closed_by_user"
        } else {
            "reopened_by_user"
        },
        None,
    )
    .await?;
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
