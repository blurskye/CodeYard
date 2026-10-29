//! README and license predicates use pinned Git snapshots, bounded like live code search.
use super::{
    store::{repo_store, BlobRange},
    templates,
};
use crate::backend::{
    cluster::NodeContext,
    db::{
        self,
        catalog_search::{Options, Search},
        models::Repository,
    },
};
use anyhow::Result;
use serde::Serialize;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

#[derive(Serialize)]
pub struct GitFacts {
    repo_id: i64,
    readme: Option<String>,
    pub license: Option<&'static str>,
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
    };
    let store = repo_store(repo, cluster);
    if store.refs().await?.is_empty() {
        return Ok(facts);
    }
    let revision = store.commit(&repo.default_branch).await?.sha;
    let mut entries = store.tree_metadata(&revision, "").await?;
    entries.sort_by_key(|e| e.name.to_ascii_lowercase());
    for entry in entries {
        if entry.entry_type != "file" {
            continue;
        }
        let name = entry.name.to_ascii_lowercase();
        let is_readme =
            readme && facts.readme.is_none() && (name == "readme" || name.starts_with("readme."));
        let is_license = facts.license.is_none() && templates::is_license_file(&name);
        if !is_readme && !is_license {
            continue;
        }
        let blob = store
            .blob_range(
                &revision,
                &entry.path,
                BlobRange::Window {
                    offset: 0,
                    length: 256 * 1024,
                },
            )
            .await?;
        // An oversized README or license stays unknown instead of failing the whole search.
        if blob.size > 256 * 1024 {
            continue;
        }
        if is_readme {
            facts.readme = std::str::from_utf8(&blob.bytes).ok().map(str::to_owned);
        }
        if is_license {
            facts.license = templates::detect_license(&blob.bytes).map(|t| t.name);
        }
    }
    Ok(facts)
}

pub async fn prepare(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    viewer: Option<i64>,
    query: Search,
) -> Result<(Search, bool)> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let candidates = db::catalog_search::search(
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
    let repos = db::repositories::records::get_repositories_by_ids(pool, &ids).await?;
    let mut facts = Vec::new();
    let mut bytes = 0;
    for repo in repos {
        // Search candidates are not authority to read a later Git snapshot.
        if super::access::check_read(pool, repo.id, viewer)
            .await
            .is_err()
        {
            incomplete = true;
            continue;
        }
        match timeout_at(deadline, git_facts(&repo, cluster, true)).await {
            Ok(Ok(value)) => {
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
