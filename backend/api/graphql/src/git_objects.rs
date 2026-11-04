//! Immutable Git node lookup and refs use RepoStore, including remote repositories.
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use base64::engine::general_purpose::STANDARD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use codeyard_app::repositories::store::repo_store;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

fn key(value: &str) -> Option<(&str, Vec<u8>)> {
    let (kind, value) = value.split_once('_')?;
    Some((kind, URL_SAFE_NO_PAD.decode(value).ok()?))
}
fn decode_reference(value: &str) -> Option<(i64, String)> {
    let (kind, bytes) = key(value)?;
    if kind != "REF" {
        return None;
    }
    // Accept the textual key emitted by early REST refs as well as today's shared encoding.
    if let Ok(text) = std::str::from_utf8(&bytes) {
        if let Some((repo, name)) = text.split_once('/') {
            if let Ok(repo) = repo.parse::<i64>() {
                if repo > 0 && name.starts_with("refs/") {
                    return Some((repo, name.to_owned()));
                }
            }
        }
    }
    let (id, name) = bytes.split_at_checked(8)?;
    let id = i64::from_be_bytes(id.try_into().ok()?);
    let name = std::str::from_utf8(name).ok()?;
    (id > 0 && name.starts_with("refs/")).then(|| (id, name.to_owned()))
}

pub(super) async fn load(
    repo: &codeyard_db_core::models::Repository,
    oid: &str,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let store = repo_store(repo, request.state.cluster.as_ref());
    let kind = store.git_object_kind(oid).await.map_err(|e| {
        if e.kind == ErrorKind::NotFound {
            missing()
        } else {
            unavailable(e)
        }
    })?;
    match kind.as_str() {
        "commit" => crate::git::load(repo, oid, "contents", request).await,
        "blob" => {
            let blob = store
                .blob_by_id_range(
                    oid,
                    codeyard_storage::range::ByteRange::Window {
                        offset: 0,
                        length: 0,
                    },
                )
                .await
                .map_err(unavailable)?;
            request.charge(blob.size.min(8_000) as usize)?;
            Ok(
                json!({"__typename":"Blob","_repo":repo.id,"id":node_id::git_object("B",repo.id,oid),"oid":oid,"byteSize":blob.size,
                "isBinary":blob.is_binary,"isTruncated":blob.size > codeyard_repos_core::store_types::MAX_BLOB_RESPONSE_BYTES as u64}),
            )
        }
        "tree" => {
            let tree = store.git_tree(oid, false).await.map_err(unavailable)?;
            if tree.truncated {
                return Err(async_graphql::Error::new(
                    "Tree exceeds the repository store read limit",
                ));
            }
            request.charge(tree.tree.iter().fold(0usize, |bytes, entry| {
                bytes
                    .saturating_add(entry.path.len().saturating_mul(6))
                    .saturating_add(256)
            }))?;
            Ok(
                json!({"__typename":"Tree","_repo":repo.id,"id":node_id::git_object("T",repo.id,oid),"oid":oid,"entries":tree.tree.iter().map(|e|json!({"_repo":repo.id,"name":e.path,"nameRaw":STANDARD.encode(&e.path_raw),"path":e.path,"pathRaw":STANDARD.encode(&e.path_raw),"submodule":null,"mode":i32::from_str_radix(&e.mode,8).unwrap_or(0),"oid":e.sha,"size":e.size.unwrap_or(0),"type":e.kind})).collect::<Vec<_>>()}),
            )
        }
        "tag" => {
            let tag = store.git_tag(oid).await.map_err(unavailable)?;
            request.charge(
                tag.message
                    .len()
                    .saturating_add(tag.tag.len())
                    .saturating_add(tag.object.len())
                    .saturating_add(tag.tagger.as_ref().map_or(0, |tagger| {
                        tagger.name.len().saturating_add(tagger.email.len())
                    })),
            )?;
            Ok(
                json!({"__typename":"Tag","_repo":repo.id,"_target":tag.object,"id":node_id::git_object("TAG",repo.id,oid),"oid":oid,"name":tag.tag,"message":tag.message,"tagger":tag.tagger.as_ref().map(|t|json!({"name":t.name,"email":t.email,"date":t.date,"user":null}))}),
            )
        }
        _ => Err(missing()),
    }
}

/// GitHub answers `object: null` when an expression names no object.
fn absent<T>(result: std::result::Result<T, RepoError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {
            Ok(None)
        }
        Err(error) => Err(unavailable(error)),
    }
}

pub(super) async fn blob_text(parent: &Value, request: &RequestContext) -> Result<Value> {
    if parent["isBinary"] == true || parent["isTruncated"] == true {
        return Ok(Value::Null);
    }
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    request.authorize(repo.id, "contents", false).await?;
    let bytes = repo_store(&repo, request.state.cluster.as_ref())
        .blob_by_id(parent["oid"].as_str().ok_or_else(missing)?)
        .await
        .map_err(unavailable)?;
    request.charge(bytes.len())?;
    Ok(std::str::from_utf8(&bytes)
        .ok()
        .map_or(Value::Null, |text| json!(text)))
}

// Callers authorize before I/O; recheck after it and charge the map plus cloned names.
async fn loaded_refs(
    repo: &codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<std::collections::HashMap<String, String>> {
    let refs = repo_store(repo, request.state.cluster.as_ref())
        .refs()
        .await
        .map_err(unavailable)?;
    request.authorize(repo.id, "contents", false).await?;
    request.charge(
        refs.iter()
            .map(|(name, oid)| name.len() * 2 + oid.len() + 96)
            .sum(),
    )?;
    Ok(refs)
}

pub(super) async fn reference(
    repo: &codeyard_db_core::models::Repository,
    name: &str,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let refs = loaded_refs(repo, request).await?;
    let Some(oid) = refs.get(name) else {
        return Ok(Value::Null);
    };
    reference_at(repo, name, oid, request).await
}

pub(super) async fn reference_at(
    repo: &codeyard_db_core::models::Repository,
    name: &str,
    oid: &str,
    _request: &RequestContext,
) -> Result<Value> {
    let (prefix, short) = if let Some(name) = name.strip_prefix("refs/heads/") {
        ("refs/heads/", name)
    } else if let Some(name) = name.strip_prefix("refs/tags/") {
        ("refs/tags/", name)
    } else {
        ("refs/", name.trim_start_matches("refs/"))
    };
    Ok(
        json!({"__typename":"Ref","_repo":repo.id,"_oid":oid,"id":node_id::reference(repo.id,name),"name":short,"prefix":prefix}),
    )
}

pub(super) async fn reference_field(
    field: &str,
    args: &crate::arguments::Arguments,
    parent: &Value,
    request: &RequestContext,
) -> Result<Value> {
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    if field == "target" {
        return load(&repo, parent["_oid"].as_str().ok_or_else(missing)?, request).await;
    }
    let branch = parent["name"].as_str().ok_or_else(missing)?;
    if field == "branchProtectionRule" {
        let rules = codeyard_repos::protection::matching_rules(&request.state.pool, &repo, branch)
            .await
            .map_err(unavailable)?;
        if parent["prefix"] != "refs/heads/" || rules.is_empty() {
            return Ok(Value::Null);
        }
        // Native protection combines matching rules, rather than choosing one pattern.
        return Ok(json!({
            "requiresStrictStatusChecks":rules.iter().any(|r| r.require_up_to_date_branch),
            "requiredApprovingReviewCount":rules.iter().map(|r| r.required_approving_reviews).max(),
        }));
    }
    let head = crate::arg(args, "headRef")?;
    let (source, head) = if let Some((owner, branch)) = head.split_once(':') {
        let source = codeyard_app::repositories::pull_requests::branches::owner_head_repository(
            &request.state.pool,
            &repo,
            owner,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        let source = crate::repository::by_id(source.id, request).await?;
        if !codeyard_app::repositories::reads::same_fork_network(
            &request.state.pool,
            &repo,
            &source,
        )
        .await
        .map_err(unavailable)?
        {
            return Err(missing());
        }
        (source, branch.to_owned())
    } else {
        (repo.clone(), head)
    };
    request.authorize(source.id, "contents", false).await?;
    let store = repo_store(&repo, request.state.cluster.as_ref());
    let comparison = store
        .comparison_from(
            &repo_store(&source, request.state.cluster.as_ref()),
            parent["_oid"].as_str().ok_or_else(missing)?,
            &format!("refs/heads/{head}"),
            0,
            0,
        )
        .await
        .map_err(unavailable)?;
    Ok(json!({
        "aheadBy":comparison.ahead_by,"behindBy":comparison.behind_by,
        "status":match (comparison.ahead_by,comparison.behind_by) {
            (0,0)=>"IDENTICAL",(_,0)=>"AHEAD",(0,_)=>"BEHIND",_=>"DIVERGED",
        },
    }))
}

pub(super) async fn node(value: &str, request: &RequestContext) -> Result<Value> {
    let (kind, bytes) = key(value).ok_or_else(missing)?;
    if matches!(kind, "SCR" | "STAT") {
        let (repo, oid) = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.split_once('/'))
            .ok_or_else(missing)?;
        let repo = repo
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(missing)?;
        if !codeyard_git::is_full_commit_id(oid) {
            return Err(missing());
        }
        crate::repository::by_id(repo, request).await?;
        request.authorize(repo, "statuses", false).await?;
        return crate::git::resolve(
            "Commit",
            if kind == "STAT" {
                "status"
            } else {
                "statusCheckRollup"
            },
            &crate::arguments::Arguments::new([]),
            request,
            &json!({"_repo":repo,"oid":oid}),
        )
        .await;
    }
    if kind == "RR" {
        let value = std::str::from_utf8(&bytes).map_err(|_| missing())?;
        let (pull, user) = value.split_once('/').ok_or_else(missing)?;
        let pull = pull
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(missing)?;
        let user = user
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(missing)?;
        Box::pin(crate::node(&node_id::encode("PR", pull), request)).await?;
        let row = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
            &request.state.pool,
            pull,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        let reviewer = codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(
            &request.state.pool,
            pull,
        )
        .await
        .map_err(unavailable)?
        .into_iter()
        .find(|row| row.id == user)
        .ok_or_else(missing)?;
        return crate::pull_request::review_request(&row, &reviewer, request).await;
    }
    if kind == "REF" {
        let (id, name) = decode_reference(value).ok_or_else(missing)?;
        let repo = crate::repository::by_id(id, request).await?;
        return reference(&repo, &name, request).await;
    }
    if !matches!(kind, "C" | "B" | "TAG" | "T") {
        return Err(missing());
    }
    let key = std::str::from_utf8(&bytes).map_err(|_| missing())?;
    let (repo, oid) = if let Some((repo, oid)) = key.split_once('/') {
        (
            repo.parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(missing)?,
            oid,
        )
    } else if request.auth.is_job() {
        // Only a single-repository job credential gives an old, unqualified ID an unambiguous
        // origin. Other legacy IDs cannot retain repository-specific identity, so fail closed
        // instead of selecting a different fork or probing every readable repository.
        let token = codeyard_db_actions::ci_tokens::get(&request.state.pool, request.auth.token.id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        (token.repo_id, key)
    } else {
        return Err(missing());
    };
    if !codeyard_git::is_full_commit_id(oid) {
        return Err(missing());
    }
    let repo = crate::repository::by_id(repo, request).await?;
    checked(&repo, kind, oid, request).await
}
pub(super) async fn resolve(
    field: &str,
    args: &crate::arguments::Arguments,
    repo: &codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let store = repo_store(repo, request.state.cluster.as_ref());
    match field {
        "ref" => {
            let name = args.args.try_get("qualifiedName")?.string()?.to_owned();
            let name = if name.starts_with("refs/") {
                name
            } else {
                format!("refs/heads/{name}")
            };
            reference(repo, &name, request).await
        }
        "object" => {
            if let Some(oid) = args.args.get("oid").filter(|v| !v.is_null()) {
                return load(repo, oid.string()?, request).await;
            }
            let Some(expression) = args.args.get("expression").filter(|v| !v.is_null()) else {
                return Ok(Value::Null);
            };
            let expression = expression.string()?;
            let mut path = "";
            let oid = if let Some((revision, tree_path)) = expression.split_once(':') {
                path = tree_path.trim_end_matches('/');
                if path.is_empty() {
                    absent(store.git_tree(revision, false).await)?.map(|tree| tree.sha)
                } else {
                    let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                    absent(store.tree_metadata(revision, parent).await)?
                        .and_then(|entries| entries.into_iter().find(|entry| entry.path == path))
                        .map(|entry| entry.sha)
                }
            } else if codeyard_git::is_full_commit_id(expression) {
                Some(expression.to_owned())
            } else {
                let refs = loaded_refs(repo, request).await?;
                if let Some(oid) = refs
                    .get(expression)
                    .or_else(|| refs.get(&format!("refs/heads/{expression}")))
                    .or_else(|| refs.get(&format!("refs/tags/{expression}")))
                {
                    Some(oid.clone())
                } else {
                    absent(store.commit(expression).await)?.map(|commit| commit.sha)
                }
            };
            let Some(oid) = oid else {
                return Ok(Value::Null);
            };
            let mut value = load(repo, &oid, request).await?;
            // TreeEntry.path is the full path from the repository root, as on GitHub.
            if !path.is_empty() {
                if let Some(entries) = value["entries"].as_array_mut() {
                    for entry in entries {
                        let name = entry["name"].as_str().unwrap_or_default();
                        let raw = STANDARD
                            .decode(entry["nameRaw"].as_str().unwrap_or_default())
                            .unwrap_or_default();
                        entry["path"] = json!(format!("{path}/{name}"));
                        entry["pathRaw"] =
                            json!(STANDARD.encode([path.as_bytes(), b"/", &raw].concat()));
                    }
                }
            }
            Ok(value)
        }
        "refs" => {
            crate::connection::validate(args)?;
            let prefix = args.args.try_get("refPrefix")?.string()?.to_owned();
            let query = args
                .args
                .get("query")
                .filter(|v| !v.is_null())
                .map(|v| v.string())
                .transpose()?
                .unwrap_or("");
            let order = args
                .args
                .get("orderBy")
                .filter(|v| !v.is_null())
                .map(|v| v.deserialize::<Value>())
                .transpose()?;
            if order
                .as_ref()
                .and_then(|v| v["field"].as_str())
                .is_some_and(|field| field != "ALPHABETICAL")
            {
                return Err(async_graphql::Error::new(
                    "Codeyard does not support this ref ordering yet",
                ));
            }
            let refs = loaded_refs(repo, request).await?;
            let mut names = refs
                .keys()
                .filter(|name| name.starts_with(&prefix) && name[prefix.len()..].contains(query))
                .cloned()
                .collect::<Vec<_>>();
            names.sort();
            let direction = order.as_ref().and_then(|v| v["direction"].as_str()).or(args
                .args
                .get("direction")
                .filter(|v| !v.is_null())
                .map(|v| v.enum_name())
                .transpose()?);
            if direction == Some("DESC") {
                names.reverse();
            }
            let page = crate::connection::Page::new(args, names.len() as i64)?;
            let mut nodes = vec![];
            for name in names
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                nodes.push(reference_at(repo, &name, &refs[&name], request).await?);
            }
            Ok(page.finish(nodes))
        }
        _ => Err(missing()),
    }
}
async fn checked(
    repo: &codeyard_db_core::models::Repository,
    kind: &str,
    oid: &str,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let value = load(repo, oid, request).await?;
    if matches!(
        (kind, value["__typename"].as_str()),
        ("C", Some("Commit")) | ("B", Some("Blob")) | ("TAG", Some("Tag")) | ("T", Some("Tree"))
    ) {
        Ok(value)
    } else {
        Err(missing())
    }
}
