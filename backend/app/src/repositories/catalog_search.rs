//! README and license predicates use pinned Git snapshots, bounded like live code search.
use crate::cluster::context::NodeContext;
use crate::repositories::store::repo_store;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_repos::catalog_search::Options;
use codeyard_db_repos::catalog_search::Search;
use codeyard_storage::range::ByteRange as BlobRange;
use serde::Serialize;
use std::time::Duration;
use tokio::time::timeout_at;
use tokio::time::Instant;

#[derive(Serialize)]
pub struct GitFacts {
    repo_id: i64,
    readme: Option<String>,
    pub license: Option<&'static str>,
    #[serde(skip)]
    readme_incomplete: bool,
}

pub async fn git_facts(
    repo: &Repository,
    cluster: Option<&NodeContext>,
    readme: bool,
) -> Result<GitFacts> {
    let mut facts = GitFacts {
        repo_id: repo.id,
        readme: None,
        license: None,
        readme_incomplete: false,
    };
    let store = repo_store(repo, cluster);
    let refs = store.refs_from_primary().await?;
    let Some(revision) = refs.get(&format!("refs/heads/{}", repo.default_branch)) else {
        return Ok(facts);
    };
    let directories = crate::repositories::community::directories(&store, revision).await?;
    let mut read_readme = false;
    let mut read_license = false;
    for (_, mut entries) in directories {
        entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
        for entry in entries {
            if entry.entry_type != "file" {
                continue;
            }
            let name = entry.name.to_ascii_lowercase();
            let is_readme =
                readme && !read_readme && (name == "readme" || name.starts_with("readme."));
            let is_license = !read_license && codeyard_repos::templates::is_license_file(&name);
            if !is_readme && !is_license {
                continue;
            }
            let blob = store
                .blob_range(
                    revision,
                    &entry.path,
                    BlobRange::Window {
                        offset: 0,
                        length: 256 * 1024,
                    },
                )
                .await?;
            read_readme |= is_readme;
            read_license |= is_license;
            // An unread README is unknown, not an authoritative negative search predicate.
            if blob.size > 256 * 1024 {
                facts.readme_incomplete |= is_readme;
                continue;
            }
            if is_readme && !blob.is_binary && !blob.bytes.contains(&0) {
                facts.readme = std::str::from_utf8(&blob.bytes).ok().map(str::to_owned);
            }
            if is_license {
                facts.license =
                    codeyard_repos::templates::detect_license(&blob.bytes).map(|t| t.name);
            }
        }
    }
    Ok(facts)
}

pub async fn prepare(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    viewer: Option<i64>,
    query: Search,
) -> Result<(Search, bool)> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let candidates = codeyard_db_repos::catalog_search::search(
        pool,
        viewer,
        &query.git_candidates(),
        &Options {
            repository: None,
            sort: Some("updated".into()),
            ascending: false,
            offset: 0,
            limit: 250,
        },
    )
    .await?;
    let mut incomplete = candidates.total > candidates.items.len() as i64;
    let ids = candidates
        .items
        .iter()
        .filter_map(|hit| hit.repository_id)
        .collect::<Vec<_>>();
    let repos =
        codeyard_db_repos::repositories::records::get_repositories_by_ids(pool, &ids).await?;
    let mut facts = Vec::new();
    let mut bytes = 0;
    let readme = query.needs_readme();
    for repo in repos {
        // Search candidates are not authority to read a later Git snapshot.
        if codeyard_identity::repository_access::check_read(pool, repo.id, viewer)
            .await
            .is_err()
        {
            incomplete = true;
            continue;
        }
        match timeout_at(deadline, git_facts(&repo, cluster, readme)).await {
            Ok(Ok(value)) => {
                if value.readme_incomplete {
                    incomplete = true;
                    continue;
                }
                bytes += value.readme.as_ref().map_or(0, String::len);
                if bytes > 8 * 1024 * 1024 {
                    incomplete = true;
                    break;
                }
                facts.push(value);
            }
            _ => {
                incomplete = true;
            }
        }
        if Instant::now() >= deadline {
            incomplete = true;
            break;
        }
    }
    Ok((query.with_git(serde_json::to_string(&facts)?), incomplete))
}
