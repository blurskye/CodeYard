//! OSV mirrors and repository alerts share the pinned dependency inventory.
use anyhow::ensure;
use anyhow::Result;
use codeyard_git::error::ErrorKind;
use codeyard_repos_pulls::services::RepositoryServices;
use codeyard_types::vulnerabilities::Advisory;
use codeyard_types::vulnerabilities::PreparedAdvisory;
use codeyard_types::vulnerabilities::ECOSYSTEMS;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

pub async fn ingest(pool: &codeyard_database::DbPool, bytes: &[u8]) -> Result<bool> {
    let bytes = bytes.to_vec();
    let record = tokio::task::spawn_blocking(move || Advisory::parse(&bytes)?.prepare()).await??;
    let changed = codeyard_db_security::vulnerabilities::upsert(pool, &record).await?;
    if changed {
        codeyard_db_security::vulnerabilities::rescan_all(pool).await?;
    }
    Ok(changed)
}
pub async fn scan(
    state: &dyn RepositoryServices,
    repo: &codeyard_database::models::Repository,
    generation: i64,
) -> Result<()> {
    let reader = codeyard_db_security::dependency_graph::Reader::Scan(generation);
    let reference = format!("refs/heads/{}", repo.default_branch);
    let (sha, packages) =
        match codeyard_repos_pulls::repositories::dependency_graph::native_inventory(
            state, &reader, repo, &reference,
        )
        .await
        {
            Ok(result) => result,
            Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
    let snapshot =
        codeyard_repos_pulls::repositories::dependency_graph::snapshot(sha, reference, packages);
    codeyard_db_security::vulnerabilities::save_snapshot(
        state.pool(),
        repo.id,
        generation,
        &snapshot,
    )
    .await
}
async fn sync(state: &dyn RepositoryServices) -> Result<()> {
    let settings = codeyard_db_security::vulnerabilities::settings(state.pool()).await?;
    synchronize(state.pool(), &settings).await
}
/// Downloads each enabled ecosystem's `all.zip` from the OSV mirror and stores its records.
pub async fn synchronize(
    pool: &codeyard_database::DbPool,
    settings: &codeyard_db_security::vulnerabilities::Settings,
) -> Result<()> {
    let enabled: Vec<String> = serde_json::from_str(&settings.ecosystems)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut failures = Vec::new();
    for ecosystem in enabled {
        let result = async {
            ensure!(
                ECOSYSTEMS.contains(&ecosystem.as_str()),
                "Unknown OSV ecosystem"
            );
            let url = format!(
                "{}/{}/all.zip",
                settings.base_url.trim_end_matches('/'),
                ecosystem
            );
            let mut response = client.get(url).send().await?.error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= 256 * 1024 * 1024,
                    "OSV archive exceeds its limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            let records = tokio::task::spawn_blocking(move || -> Result<Vec<PreparedAdvisory>> {
                let mut archive = codeyard_storage::zip_archives::open(bytes, 1_000_000)?;
                let mut records = Vec::new();
                let mut expanded = 0u64;
                for i in 0..archive.len() {
                    let mut entry = archive.by_index(i)?;
                    if entry.is_dir() || !entry.name().ends_with(".json") {
                        continue;
                    }
                    ensure!(
                        entry.size() <= 4 * 1024 * 1024,
                        "OSV record exceeds its limit"
                    );
                    let declared = entry.size();
                    let mut bytes = Vec::new();
                    let remaining = (2u64 * 1024 * 1024 * 1024).saturating_sub(expanded);
                    entry
                        .by_ref()
                        .take((4 * 1024 * 1024 + 1).min(remaining.saturating_add(1)))
                        .read_to_end(&mut bytes)?;
                    expanded = expanded
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Expanded OSV archive exceeds its limit"))?;
                    ensure!(
                        expanded <= 2 * 1024 * 1024 * 1024,
                        "Expanded OSV archive exceeds its limit"
                    );
                    ensure!(
                        bytes.len() as u64 == declared,
                        "OSV archive entry size mismatch"
                    );
                    // One record the catalogue cannot hold must not stop the rest of the archive.
                    match Advisory::parse(&bytes).and_then(Advisory::prepare) {
                        Ok(record) => records.push(record),
                        Err(error) => {
                            tracing::warn!(entry = entry.name(), %error, "OSV record was skipped")
                        }
                    }
                }
                Ok(records)
            })
            .await??;
            // Repositories are rescanned once per archive, not once per changed record.
            let mut changed = false;
            for record in records {
                changed |= codeyard_db_security::vulnerabilities::upsert(pool, &record).await?;
            }
            if changed {
                codeyard_db_security::vulnerabilities::rescan_all(pool).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%ecosystem, %error, "OSV ecosystem synchronization failed");
            failures.push(ecosystem);
        }
    }
    ensure!(
        failures.is_empty(),
        "OSV synchronization failed for {}",
        failures.join(", ")
    );
    Ok(())
}
pub async fn run(
    state: Arc<impl RepositoryServices + 'static>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut discovered = false;
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {_ = stop.changed()=>break,_=interval.tick()=>{}}
        let work = async {
            if codeyard_db_security::vulnerabilities::claim_sync(state.pool()).await? {
                if let Err(error) = sync(&state).await {
                    tracing::warn!(%error,"OSV synchronization failed");
                }
            }
            if !discovered {
                codeyard_db_security::vulnerabilities::queue_changed(state.pool()).await?;
                discovered = true;
            }
            for _ in 0..20 {
                let Some((repo, generation)) =
                    codeyard_db_security::vulnerabilities::claim(state.pool()).await?
                else {
                    break;
                };
                if let Some(repo) = codeyard_db_repos::repositories::records::get_repository_by_id(
                    state.pool(),
                    repo,
                )
                .await?
                {
                    match tokio::time::timeout(
                        Duration::from_secs(540),
                        scan(&state, &repo, generation),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(repo_id=repo.id,%error,"Dependency vulnerability scan failed")
                        }
                        Err(_) => tracing::warn!(
                            repo_id = repo.id,
                            "Dependency vulnerability scan exceeded its lease budget"
                        ),
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {_ = stop.changed()=>break,result=work=>if let Err(error)=result{tracing::warn!(%error,"Vulnerability scanning job failed");}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_types::repositories::Package;
    use codeyard_types::vulnerabilities::in_range;
    use codeyard_types::vulnerabilities::OsvPackage;
    use serde_json::json;
    #[test]
    fn ranges_are_checked_one_entry_at_a_time() {
        assert!(in_range("crates.io", "1.1.0", ">= 1.0.0, < 1.2.0"));
        assert!(!in_range("crates.io", "1.1.0", ">= 2.0.0, < 3.0.0"));
        assert!(in_range("crates.io", "2.0.1", ">= 2.1.0 || = 2.0.1"));
        assert!(in_range("crates.io", "0.1.0", ">= 0, < 1.0.0"));
        assert!(!in_range("crates.io", "1.0.0", "unknown"));
    }
    #[test]
    fn osv_catalogue_and_snapshot_matching() {
        let mut advisory = Advisory::parse(include_bytes!("fixtures/OSV-2026-1.json")).unwrap();
        advisory.aliases.clear();
        advisory.affected[0].package = OsvPackage {
            ecosystem: "PyPI".into(),
            name: "Example_Package".into(),
        };
        advisory.affected[0].ranges[0].r#type = "ECOSYSTEM".into();
        advisory.affected[0].ranges[0].events =
            vec![json!({"introduced":"0"}), json!({"fixed":"1.0"})];
        let document = advisory.global_document().unwrap();
        assert_eq!(
            document["ghsa_id"],
            advisory.global_document().unwrap()["ghsa_id"]
        );
        assert_eq!(
            document["references"],
            json!(["https://example.test/advisory"])
        );
        let package = Package {
            manifest: "requirements.txt".into(),
            ecosystem: "pypi".into(),
            name: "example-package".into(),
            version: "1.0rc1".into(),
            package_url: "pkg:pypi/example-package@1.0rc1".into(),
            scope: "runtime".into(),
        };
        let snapshot = codeyard_repos_pulls::repositories::dependency_graph::snapshot(
            "a".repeat(40),
            "refs/heads/main".into(),
            vec![package],
        );
        snapshot.validate().unwrap();
        let mut osv = std::collections::BTreeMap::from([(
            document["ghsa_id"].as_str().unwrap().to_owned(),
            advisory,
        )]);
        let matches = codeyard_database::queries::dependabot_alerts::matches(
            std::slice::from_ref(&snapshot),
            std::slice::from_ref(&document),
            &osv,
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.version, "1.0rc1");
        osv.values_mut().next().unwrap().withdrawn = Some(chrono::Utc::now());
        assert!(codeyard_database::queries::dependabot_alerts::matches(
            &[snapshot],
            &[document],
            &osv
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn local_maven_advisories_match_namespaced_package_urls() {
        let document = json!({"ghsa_id":"GHSA-aaaa-bbbb-cccc","severity":"high","withdrawn_at":null,
            "vulnerabilities":[{"package":{"ecosystem":"maven","name":"org.example:parser"},
                "vulnerable_version_range":"< 2.0.0"}]});
        let package = Package {
            manifest: "pom.xml".into(),
            ecosystem: "maven".into(),
            name: "org.example/parser".into(),
            version: "1.0.0".into(),
            package_url: "pkg:maven/org.example/parser@1.0.0".into(),
            scope: "runtime".into(),
        };
        let snapshot = codeyard_repos_pulls::repositories::dependency_graph::snapshot(
            "a".repeat(40),
            "refs/heads/main".into(),
            vec![package],
        );
        let matches = codeyard_database::queries::dependabot_alerts::matches(
            &[snapshot],
            &[document],
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
    }
    #[test]
    fn osv_ranges_and_withdrawal() {
        let mut a = Advisory::parse(include_bytes!("fixtures/OSV-2026-1.json")).unwrap();
        let mut p = Package {
            manifest: "Cargo.lock".into(),
            ecosystem: "cargo".into(),
            name: "example".into(),
            version: "1.0.0".into(),
            package_url: "pkg:cargo/example@1.0.0".into(),
            scope: "runtime".into(),
        };
        assert!(a.matches(&p));
        p.version = "1.2.0".into();
        assert!(!a.matches(&p));
        p.version = "1.2.0-rc.1".into();
        assert!(a.matches(&p));
        a.withdrawn = Some(chrono::Utc::now());
        assert!(!a.matches(&p));
    }
}
