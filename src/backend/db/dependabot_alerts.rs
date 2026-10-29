//! Local advisories and snapshot matches. No advisory read contacts an external feed.
use super::{
    advisories::{self, Advisory, Data, Refusal},
    DbPool,
};
use crate::backend::dependency_graph::{self, Package, Snapshot};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Only lightweight filter metadata is streamed; complete documents are loaded for this page.
pub struct CatalogueQuery {
    pub filters: std::collections::HashMap<String, String>,
    pub sort: String,
    pub ascending: bool,
    pub cursor: Option<(String, String)>,
    pub before: bool,
    pub offset: usize,
    pub limit: usize,
}
pub struct CataloguePage {
    pub documents: Vec<Value>,
    pub previous: bool,
    pub next: bool,
}
pub(crate) struct CatalogueSelection<'a> {
    query: &'a CatalogueQuery,
    ids: Vec<String>,
    skipped: usize,
}
impl<'a> CatalogueSelection<'a> {
    pub(crate) fn new(query: &'a CatalogueQuery, offset_applied: bool) -> Self {
        Self { query, ids: Vec::new(), skipped: if offset_applied { query.offset } else { 0 } }
    }
    pub(crate) fn push(&mut self, id: String, metadata: Value) -> bool {
        let q = self.query;
        if !catalogue_matches(&metadata, &q.filters) { return true; }
        if let Some(cursor) = &q.cursor {
            let key = (metadata[if q.sort == "published" { "published_at" } else { "updated_at" }].as_str().unwrap_or("").to_owned(), id.clone());
            let beyond = if q.ascending ^ q.before { key > *cursor } else { key < *cursor };
            if !beyond { return true; }
        } else if self.skipped < q.offset {
            self.skipped += 1;
            return true;
        }
        self.ids.push(id);
        self.ids.len() <= q.limit
    }
    pub(crate) fn finish(mut self) -> (Vec<String>, bool, bool) {
        let more = self.ids.len() > self.query.limit;
        self.ids.truncate(self.query.limit);
        if self.query.before {
            self.ids.reverse();
            (self.ids, more, self.query.cursor.is_some())
        } else {
            (self.ids, self.query.offset > 0 || self.query.cursor.is_some(), more)
        }
    }
}

pub(crate) fn catalogue_statement(pg: bool, q: &CatalogueQuery) -> (String, Vec<String>, bool) {
    let text = |field: &str| if pg { format!("doc::jsonb->>'{field}'") } else { format!("json_extract(doc,'$.{field}')") };
    let json = |field: &str| if pg { format!("doc::jsonb->'{field}'") } else { format!("json_extract(doc,'$.{field}')") };
    let timestamp = |field: &str| if pg {
        format!("to_char({field} AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS') || CASE WHEN EXTRACT(MICROSECONDS FROM {field})::bigint%1000000=0 THEN 'Z' WHEN EXTRACT(MICROSECONDS FROM {field})::bigint%1000=0 THEN to_char({field} AT TIME ZONE 'UTC','.MS\"Z\"') ELSE to_char({field} AT TIME ZONE 'UTC','.US\"Z\"') END")
    } else {
        format!("CASE WHEN substr({field},21,6)='000000' THEN substr({field},1,19)||'Z' WHEN substr({field},24,3)='000' THEN substr({field},1,23)||'Z' ELSE {field} END")
    };
    let published = timestamp("a.published_at");
    let updated = timestamp("a.updated_at");
    let imported_published = text("published_at").replace("doc", "document");
    let imported_updated = text("updated_at").replace("doc", "document");
    let cve = json("cve_id");
    let severity = text("severity");
    let kind = text("type");
    let withdrawn = json("withdrawn_at");
    let vulnerabilities = json("vulnerabilities");
    let cwes = json("cwes");
    let eligible = if pg {
        "SELECT COALESCE(jsonb_agg(v),'[]'::jsonb) FROM jsonb_array_elements(COALESCE(doc::jsonb->'vulnerabilities','[]'::jsonb)) v WHERE v->'package'->>'name' IS NOT NULL AND v->>'vulnerable_version_range' IS NOT NULL"
    } else {
        "SELECT json_group_array(json(value)) FROM json_each(doc,'$.vulnerabilities') WHERE json_extract(value,'$.package.name') IS NOT NULL AND json_extract(value,'$.vulnerable_version_range') IS NOT NULL"
    };
    let metadata = if pg {
        format!("jsonb_build_object('ghsa_id',ghsa_id,'cve_id',{cve},'severity',COALESCE({severity},'unknown'),'type',CASE WHEN local THEN 'reviewed' ELSE {kind} END,'withdrawn_at',CASE WHEN local THEN NULL ELSE {withdrawn} END,'published_at',published_at,'updated_at',updated_at,'vulnerabilities',CASE WHEN local THEN ({eligible}) ELSE {vulnerabilities} END,'cwes',CASE WHEN local THEN NULL ELSE {cwes} END)::text")
    } else {
        format!("json_object('ghsa_id',ghsa_id,'cve_id',{cve},'severity',COALESCE({severity},'unknown'),'type',CASE WHEN local THEN 'reviewed' ELSE {kind} END,'withdrawn_at',CASE WHEN local THEN NULL ELSE {withdrawn} END,'published_at',published_at,'updated_at',updated_at,'vulnerabilities',json(CASE WHEN local THEN ({eligible}) ELSE {vulnerabilities} END),'cwes',json(CASE WHEN local THEN NULL ELSE {cwes} END))")
    };
    let field = if q.sort == "published" { "published_at" } else { "updated_at" };
    let direction = if q.ascending ^ q.before { "ASC" } else { "DESC" };
    let collation = if pg { "\"C\"" } else { "BINARY" };
    let source = format!("WITH source AS (SELECT ghsa_id,document AS doc,FALSE AS local,{imported_published} AS published_at,{imported_updated} AS updated_at FROM global_advisories UNION ALL SELECT a.ghsa_id,a.document,TRUE,{published},{updated} FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND NOT EXISTS(SELECT 1 FROM global_advisories g WHERE g.ghsa_id=a.ghsa_id)), candidates AS (SELECT ghsa_id,{metadata} AS metadata,published_at,updated_at FROM source)");
    let text = |field: &str| if pg { format!("metadata::jsonb->>'{field}'") } else { format!("json_extract(metadata,'$.{field}')") };
    let mut args = Vec::new();
    let mut bind = |value: String| { args.push(value); format!("${}", args.len()) };
    let mut predicates = Vec::new();
    for key in ["ghsa_id", "cve_id", "severity", "type"] {
        let value = q.filters.get(key).map(String::as_str).or(if key == "type" { Some("reviewed") } else { None });
        if let Some(value) = value {
            let values = value.split(',').map(|value| bind(value.to_owned())).collect::<Vec<_>>().join(",");
            predicates.push(format!("COALESCE({},'') IN ({values})", text(key)));
        }
    }
    if let Some(value) = q.filters.get("is_withdrawn") {
        predicates.push(format!("{} IS {}NULL", text("withdrawn_at"), if value == "true" { "NOT " } else { "" }));
    }
    let vulnerabilities = if pg { "jsonb_array_elements(metadata::jsonb->'vulnerabilities') v" } else { "json_each(metadata,'$.vulnerabilities') v" };
    let package = |field: &str| if pg { format!("v->'package'->>'{field}'") } else { format!("json_extract(v.value,'$.package.{field}')") };
    if let Some(ecosystem) = q.filters.get("ecosystem") {
        predicates.push(format!("EXISTS(SELECT 1 FROM {vulnerabilities} WHERE {}={})", package("ecosystem"), bind(ecosystem.clone())));
    }
    let mut version_filter = false;
    if let Some(affects) = q.filters.get("affects") {
        let names = affects.split(',').map(|value| {
            let name = if let Some((name, _)) = value.rsplit_once('@').filter(|(name, _)| !name.is_empty()) { version_filter = true; name } else { value };
            bind(name.to_owned())
        }).collect::<Vec<_>>().join(",");
        predicates.push(format!("EXISTS(SELECT 1 FROM {vulnerabilities} WHERE {} IN ({names}))", package("name")));
    }
    if let Some(cwes) = q.filters.get("cwes") {
        let ids = cwes.split(',').map(|value| bind(format!("CWE-{}", value.trim_start_matches("CWE-")))).collect::<Vec<_>>().join(",");
        let (array, id) = if pg { ("jsonb_array_elements(CASE WHEN jsonb_typeof(metadata::jsonb->'cwes')='array' THEN metadata::jsonb->'cwes' ELSE '[]'::jsonb END) c", "c->>'cwe_id'") } else { ("json_each(metadata,'$.cwes') c", "json_extract(c.value,'$.cwe_id')") };
        predicates.push(format!("EXISTS(SELECT 1 FROM {array} WHERE {id} IN ({ids}))"));
    }
    for (key, field) in [("published", "published_at"), ("updated", "updated_at"), ("modified", "updated_at")] {
        if let Some(range) = q.filters.get(key) {
            let date = format!("substr({},1,10)", text(field));
            if let Some((start, end)) = range.split_once("..") {
                if start != "*" { predicates.push(format!("{date}>={}", bind(start.to_owned()))); }
                if end != "*" { predicates.push(format!("{date}<={}", bind(end.to_owned()))); }
            } else if let Some((op, rhs)) = [">=", "<=", ">", "<"].iter().find_map(|op| range.strip_prefix(*op).map(|rhs| (*op, rhs))) {
                predicates.push(format!("{date}{op}{}", bind(rhs.to_owned())));
            } else { predicates.push(format!("{date}={}", bind(range.clone()))); }
        }
    }
    if let Some((key, id)) = &q.cursor {
        let op = if q.ascending ^ q.before { ">" } else { "<" };
        let key = bind(key.clone());
        let id = bind(id.clone());
        predicates.push(format!("({field} COLLATE {collation},ghsa_id COLLATE {collation}){op}({key},{id})"));
    }
    let predicates = predicates.join(" AND ");
    let mut sql = format!("{source} SELECT ghsa_id,metadata FROM candidates WHERE {predicates} ORDER BY {field} COLLATE {collation} {direction},ghsa_id COLLATE {collation} {direction}");
    // Version-range matching is shared Rust semantics, not a database-specific approximation.
    // In that case stream only candidate metadata, retaining at most one page of identifiers.
    if !version_filter {
        sql.push_str(&format!(" LIMIT CAST({} AS BIGINT) OFFSET CAST({} AS BIGINT)", bind((q.limit + 1).to_string()), bind(q.offset.to_string())));
    }
    (sql, args, !version_filter)
}

pub(crate) fn catalogue_matches(a: &Value, q: &std::collections::HashMap<String, String>) -> bool {
    let csv = |key: &str, value: &str| q.get(key).is_none_or(|s| s.split(',').any(|s| s == value));
    if ["ghsa_id", "cve_id", "severity"].iter().any(|key| !csv(key, a[*key].as_str().unwrap_or("")))
        || a["type"] != q.get("type").map_or("reviewed", String::as_str)
        || q.get("is_withdrawn").is_some_and(|v| (v == "true") == a["withdrawn_at"].is_null()) { return false; }
    let vulnerabilities = || a["vulnerabilities"].as_array().into_iter().flatten();
    if q.get("ecosystem").is_some_and(|ecosystem| !vulnerabilities().any(|v| v["package"]["ecosystem"] == *ecosystem)) { return false; }
    if q.get("affects").is_some_and(|affects| !affects.split(',').any(|p| {
        let (name, version) = p.rsplit_once('@').filter(|(n, _)| !n.is_empty()).map_or((p, None), |(n, v)| (n, Some(v)));
        vulnerabilities().any(|v| v["package"]["name"] == name && version.is_none_or(|version| affected(version, v["vulnerable_version_range"].as_str().unwrap_or(""))))
    })) { return false; }
    if q.get("cwes").is_some_and(|cwes| !a["cwes"].as_array().into_iter().flatten().any(|v| cwes.split(',').any(|c| v["cwe_id"] == format!("CWE-{}", c.trim_start_matches("CWE-"))))) { return false; }
    [("published", "published_at"), ("updated", "updated_at"), ("modified", "updated_at")].iter().all(|(key, field)| q.get(*key).is_none_or(|range| catalogue_date_matches(a[*field].as_str().unwrap_or(""), range)))
}
fn catalogue_date_matches(value: &str, range: &str) -> bool {
    let Some(date) = value.get(..10) else { return false; };
    if let Some((start, end)) = range.split_once("..") { return (start == "*" || date >= start) && (end == "*" || date <= end); }
    for op in [">=", "<=", ">", "<"] {
        if let Some(rhs) = range.strip_prefix(op) { return match op { ">=" => date >= rhs, "<=" => date <= rhs, ">" => date > rhs, _ => date < rhs }; }
    }
    date == range
}

pub(crate) fn manifest_digest(manifest: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(manifest.as_bytes()))
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Alert {
    pub repo_id: i64,
    pub number: i64,
    pub ghsa_id: String,
    pub manifest: String,
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    pub scope: String,
    pub state: String,
    pub advisory: String,
    pub vulnerability: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub fixed_at: Option<DateTime<Utc>>,
    pub dismissed_at: Option<DateTime<Utc>>,
    pub dismissed_by: Option<i64>,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub state: String,
    pub dismissed_reason: Option<String>,
    pub dismissed_comment: Option<String>,
}
impl Update {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.state.as_str(), "open" | "dismissed"),
            Refusal::Invalid
        );
        ensure!(
            self.dismissed_comment
                .as_ref()
                .is_none_or(|s| s.chars().count() <= 280),
            Refusal::Invalid
        );
        ensure!(
            if self.state == "dismissed" {
                self.dismissed_reason.as_deref().is_some_and(|s| {
                    matches!(
                        s,
                        "fix_started"
                            | "inaccurate"
                            | "no_bandwidth"
                            | "not_used"
                            | "tolerable_risk"
                    )
                })
            } else {
                self.dismissed_reason.is_none() && self.dismissed_comment.is_none()
            },
            Refusal::Invalid
        );
        Ok(())
    }
}

pub fn import_document(value: &Value) -> Result<Value> {
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
        .or_insert_with(|| json!(advisories::identifier()))
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
        json!(format!(
            "{}/advisories/{id}",
            crate::backend::api::github::api_url("")
        )),
    );
    value.insert(
        "html_url".into(),
        json!(format!(
            "{}/api/v3/advisories/{id}",
            crate::backend::api::http::config::link_origin()
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
    ensure!(value["cve_id"].is_null() || value["cve_id"].is_string(), Refusal::Invalid);
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
        let score = advisories::cvss_score(vector)?;
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
    ensure!(vulnerabilities.len() <= 100, Refusal::Invalid);
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
                .is_some_and(|s| !s.is_empty() && s.len() <= 255)
                && v["package"]["ecosystem"].as_str().is_some()
                && v["vulnerable_version_range"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 1024),
            Refusal::Invalid
        );
    }
    Ok(Value::Object(value))
}

pub(crate) fn published(row: Advisory) -> Result<Value> {
    let data = Data::from_row(&row)?;
    let vulnerabilities: Vec<_> = data.vulnerabilities.iter().filter(|v| v.package.name.is_some() && v.vulnerable_version_range.is_some()).map(|v| json!({"package":v.package,"vulnerable_version_range":v.vulnerable_version_range,"first_patched_version":v.patched_versions.as_ref().map(|s|json!(s)),"vulnerable_functions":v.vulnerable_functions})).collect();
    let cvss = data
        .cvss_vector_string
        .as_ref()
        .map(|v| json!({"vector_string":v,"score":advisories::cvss_score(v).ok()}));
    import_document(
        &json!({"ghsa_id":row.ghsa_id,"summary":data.summary,"description":data.description,"cve_id":data.cve_id,"severity":data.severity.as_deref().unwrap_or("unknown"),"published_at":row.published_at,"updated_at":row.updated_at,"vulnerabilities":vulnerabilities,"cvss":cvss}),
    )
}

/// The OSV (ecosystem, name) pairs an inventory can match, written the way `osv_packages`
/// stores them, as a JSON array. Matching loads only the OSV records these pairs select.
pub(crate) fn osv_keys(snapshots: &[Snapshot]) -> String {
    let keys: std::collections::BTreeSet<(&str, String)> = dependency_graph::packages(snapshots)
        .iter()
        .filter_map(|package| {
            let ecosystem =
                crate::backend::vulnerabilities::ecosystem(ecosystem(&package.ecosystem))?;
            Some((
                ecosystem,
                crate::backend::vulnerabilities::name(ecosystem, &package.name),
            ))
        })
        .collect();
    json!(keys).to_string()
}

fn ecosystem(purl: &str) -> &str {
    match purl {
        "cargo" => "rust",
        "pypi" => "pip",
        "gem" => "rubygems",
        "golang" => "go",
        _ => purl,
    }
}
/// Only concrete SemVer versions are compared; unknown version schemes are not guessed.
pub fn affected(version: &str, range: &str) -> bool {
    let Ok(version) = semver::Version::parse(version.strip_prefix('v').unwrap_or(version)) else {
        return false;
    };
    range.split("||").any(|part| {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let range = if part.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            format!("={part}")
        } else {
            part.to_owned()
        };
        semver::VersionReq::parse(&range).is_ok_and(|r| r.matches(&version))
    })
}
pub(crate) fn matches(
    snapshots: &[Snapshot],
    advisories: &[Value],
    osv: &std::collections::BTreeMap<String, crate::backend::vulnerabilities::Advisory>,
) -> Vec<(Package, Value, Value)> {
    use crate::backend::vulnerabilities;
    let normalized = |ecosystem: &str, name: &str| {
        vulnerabilities::name(vulnerabilities::ecosystem(ecosystem).unwrap_or(""), name)
    };
    // Advisory entries by ecosystem and normalized package name, so each dependency is compared
    // only with the entries that name it.
    let mut candidates: std::collections::HashMap<(String, String), Vec<_>> =
        std::collections::HashMap::new();
    for advisory in advisories {
        let osv = advisory["ghsa_id"].as_str().and_then(|id| osv.get(id));
        if !advisory["withdrawn_at"].is_null() {
            continue;
        }
        for vulnerability in advisory["vulnerabilities"].as_array().into_iter().flatten() {
            let ecosystem = vulnerability["package"]["ecosystem"]
                .as_str()
                .unwrap_or_default();
            let name = vulnerability["package"]["name"]
                .as_str()
                .unwrap_or_default();
            candidates
                .entry((ecosystem.to_owned(), normalized(ecosystem, name)))
                .or_default()
                .push((advisory, osv, vulnerability));
        }
    }
    let mut matches = std::collections::BTreeMap::new();
    for mut package in dependency_graph::packages(snapshots) {
        package.ecosystem = ecosystem(&package.ecosystem).into();
        let key = (
            package.ecosystem.clone(),
            normalized(&package.ecosystem, &package.name),
        );
        for (advisory, osv, vulnerability) in candidates.get(&key).into_iter().flatten() {
            let range = vulnerability["vulnerable_version_range"]
                .as_str()
                .unwrap_or("");
            let matched = if let Some(osv) = osv {
                let locked = crate::backend::repositories::dependency_graph::Package {
                    manifest: package.manifest.clone(),
                    ecosystem: package.ecosystem.clone(),
                    name: package.name.clone(),
                    version: package.version.clone(),
                    package_url: package.package_url.clone().unwrap_or_default(),
                    scope: package.scope.clone(),
                };
                osv.matches(&locked)
                    // The advisory matched; this entry must match too, or another entry's
                    // range and fixed version would be reported.
                    && vulnerabilities::in_range(
                        vulnerabilities::ecosystem(&package.ecosystem).unwrap_or(""),
                        &package.version,
                        range,
                    )
            } else {
                affected(&package.version, range)
            };
            if !matched {
                continue;
            }
            let key = (
                advisory["ghsa_id"].as_str().unwrap_or("").to_owned(),
                package.manifest.clone(),
                package.ecosystem.clone(),
                package.name.clone(),
            );
            matches.entry(key).or_insert_with(|| {
                (
                    package.clone(),
                    (*advisory).clone(),
                    (*vulnerability).clone(),
                )
            });
        }
    }
    matches.into_values().collect()
}

native_operation!(crate::backend::db::postgres::dependabot_alerts::global_page, crate::backend::db::sqlite::dependabot_alerts::global_page,
    fn global_page(pool: &DbPool, query: &CatalogueQuery) -> Result<CataloguePage>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::global,crate::backend::db::sqlite::dependabot_alerts::global,
    fn global(pool:&DbPool)->Result<Vec<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::without_osv,crate::backend::db::sqlite::dependabot_alerts::without_osv,
    fn without_osv(pool:&DbPool)->Result<Vec<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::global_one,crate::backend::db::sqlite::dependabot_alerts::global_one,
    fn global_one(pool:&DbPool, ghsa_id:&str)->Result<Option<Value>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::import,crate::backend::db::sqlite::dependabot_alerts::import,
    fn import(pool:&DbPool,actor:i64,value:&Value)->Result<Value>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::alerts,crate::backend::db::sqlite::dependabot_alerts::alerts,
    fn alerts(pool:&DbPool,actor:i64,repo:i64)->Result<Vec<Alert>>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::update,crate::backend::db::sqlite::dependabot_alerts::update,
    fn update(pool:&DbPool,actor:i64,repo:i64,number:i64,input:&Update)->Result<Alert>);
native_operation!(crate::backend::db::postgres::dependabot_alerts::organization_repositories,crate::backend::db::sqlite::dependabot_alerts::organization_repositories,
    fn organization_repositories(pool:&DbPool,actor:i64,org:i64)->Result<Vec<i64>>);
