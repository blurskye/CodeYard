//! Immutable Git node lookup and refs use RepoStore, including remote repositories.
use super::{db, git, missing, node_id, repository, unavailable, RequestContext, Result, Value};
use crate::backend::repositories::store::{repo_store, ErrorKind};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use serde_json::json;
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
    repo: &db::models::Repository,
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
        "commit" => git::load(repo, oid, "contents", request).await,
        "blob" => {
            let bytes = store.blob_by_id(oid).await.map_err(unavailable)?;
            request.charge(bytes.len())?;
            let text = std::str::from_utf8(&bytes)
                .ok()
                .filter(|_| !bytes.contains(&0));
            Ok(
                json!({"__typename":"Blob","_repo":repo.id,"id":node_id::git("B",oid),"oid":oid,"byteSize":bytes.len(),"isBinary":text.is_none(),"isTruncated":false,"text":text}),
            )
        }
        "tree" => {
            let tree = store.git_tree(oid, false).await.map_err(unavailable)?;
            if tree.truncated {
                return Err(async_graphql::Error::new(
                    "Tree exceeds the repository store read limit",
                ));
            }
            request.charge(tree.tree.iter().fold(0usize, |bytes, entry|
                bytes.saturating_add(entry.path.len().saturating_mul(6)).saturating_add(256)))?;
            Ok(
                json!({"__typename":"Tree","_repo":repo.id,"id":node_id::git("T",oid),"oid":oid,"entries":tree.tree.iter().map(|e|json!({"_repo":repo.id,"name":e.path,"nameRaw":STANDARD.encode(&e.path),"path":e.path,"pathRaw":STANDARD.encode(&e.path),"submodule":null,"mode":i32::from_str_radix(&e.mode,8).unwrap_or(0),"oid":e.sha,"size":e.size.unwrap_or(0),"type":e.kind})).collect::<Vec<_>>()}),
            )
        }
        "tag" => {
            let tag = store.git_tag(oid).await.map_err(unavailable)?;
            request.charge(tag.message.len().saturating_add(tag.tag.len()).saturating_add(tag.object.len())
                .saturating_add(tag.tagger.as_ref().map_or(0, |tagger| tagger.name.len().saturating_add(tagger.email.len()))))?;
            Ok(
                json!({"__typename":"Tag","_repo":repo.id,"_target":tag.object,"id":node_id::git("TAG",oid),"oid":oid,"name":tag.tag,"message":tag.message,"tagger":tag.tagger.as_ref().map(|t|json!({"name":t.name,"email":t.email,"date":t.date,"user":null}))}),
            )
        }
        _ => Err(missing()),
    }
}

pub(super) async fn reference(
    repo: &db::models::Repository,
    name: &str,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let refs = repo_store(repo, request.state.cluster.as_ref())
        .refs()
        .await
        .map_err(unavailable)?;
    let Some(oid) = refs.get(name) else {
        return Ok(Value::Null);
    };
    reference_at(repo, name, oid, request).await
}

pub(super) async fn reference_at(
    repo: &db::models::Repository,
    name: &str,
    oid: &str,
    request: &RequestContext,
) -> Result<Value> {
    let (prefix, short) = if let Some(name) = name.strip_prefix("refs/heads/") {
        ("refs/heads/", name)
    } else if let Some(name) = name.strip_prefix("refs/tags/") {
        ("refs/tags/", name)
    } else {
        ("refs/", name.trim_start_matches("refs/"))
    };
    Ok(
        json!({"__typename":"Ref","_repo":repo.id,"id":node_id::reference(repo.id,name),"name":short,"prefix":prefix,"target":load(repo,oid,request).await?}),
    )
}

pub(super) async fn reference_field(
    field: &str,
    args: &super::Arguments,
    parent: &Value,
    request: &RequestContext,
) -> Result<Value> {
    let repo = repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    let branch = parent["name"].as_str().ok_or_else(missing)?;
    if field == "branchProtectionRule" {
        let rules = crate::backend::repositories::protection::matching_rules(
            &request.state.pool,
            &repo,
            branch,
        )
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
    let head = super::arg(args, "headRef")?;
    let (source, head) = if let Some((owner, branch)) = head.split_once(':') {
        let source =
            db::repositories::records::get_repository(&request.state.pool, owner, &repo.name)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
        let source = repository::by_id(source.id, request).await?;
        if !crate::backend::repositories::reads::same_fork_network(
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
            parent["target"]["oid"].as_str().ok_or_else(missing)?,
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
        if !crate::backend::git::is_full_commit_id(oid) {
            return Err(missing());
        }
        repository::by_id(repo, request).await?;
        request.authorize(repo, "statuses", false).await?;
        return git::resolve(
            "Commit",
            if kind == "STAT" {
                "status"
            } else {
                "statusCheckRollup"
            },
            &super::Arguments::new([]),
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
        Box::pin(super::node(&node_id::encode("PR", pull), request)).await?;
        let row = db::pull_requests::records::get_pull_request_by_id(&request.state.pool, pull)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let reviewer =
            db::pull_requests::reviews::list_pull_request_reviewers(&request.state.pool, pull)
                .await
                .map_err(unavailable)?
                .into_iter()
                .find(|row| row.id == user)
                .ok_or_else(missing)?;
        return super::pull_request::review_request(&row, &reviewer, request).await;
    }
    if kind == "REF" {
        let (id, name) = decode_reference(value).ok_or_else(missing)?;
        let repo = repository::by_id(id, request).await?;
        return reference(&repo, &name, request).await;
    }
    if !matches!(kind, "C" | "B" | "TAG" | "T") {
        return Err(missing());
    }
    let oid = std::str::from_utf8(&bytes).map_err(|_| missing())?;
    if !crate::backend::git::is_full_commit_id(oid) {
        return Err(missing());
    }
    if request.auth.is_job() {
        let token = db::ci_tokens::get(&request.state.pool, request.auth.token.id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let repo = repository::by_id(token.repo_id, request).await?;
        return checked(&repo, kind, oid, request).await;
    }
    // REST's content-addressed IDs have no repository component. Find a readable copy;
    // never probe inaccessible stores, and never treat storage failure as absence.
    let mut offset = 0;
    let mut storage_error = None;
    loop {
        let rows = db::repositories::discovery::search_repositories(
            &request.state.pool,
            Some(request.auth.user.id),
            "",
            db::repositories::records::RepositorySort::Recent,
            offset,
            100,
        )
        .await
        .map_err(unavailable)?;
        if rows.is_empty() {
            return Err(storage_error.unwrap_or_else(missing));
        }
        offset += rows.len() as i64;
        for repo in rows {
            // A fine-grained token looks only in the repositories it reaches.
            if request.personal.is_some() {
                match request
                    .auth
                    .repository_scope(&request.state.pool, repo.id, "contents", false)
                    .await
                {
                    Ok(()) => {}
                    Err(super::auth::ScopeError::Unavailable) => return Err(unavailable(())),
                    Err(_) => continue,
                }
            }
            repository::by_id(repo.id, request).await?;
            match repo_store(&repo, request.state.cluster.as_ref())
                .git_object_kind(oid)
                .await
            {
                Ok(actual)
                    if matches!(
                        (kind, actual.as_str()),
                        ("C", "commit") | ("B", "blob") | ("TAG", "tag") | ("T", "tree")
                    ) =>
                {
                    return load(&repo, oid, request).await
                }
                Ok(_) => {}
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => storage_error = Some(unavailable(error)),
            }
        }
    }
}
pub(super) async fn resolve(
    field: &str,
    args: &super::Arguments,
    repo: &db::models::Repository,
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
            let oid = if let Some((revision, path)) = expression.split_once(':') {
                if path.is_empty() {
                    store
                        .git_tree(revision, false)
                        .await
                        .map_err(unavailable)?
                        .sha
                } else {
                    let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                    store
                        .tree_metadata(revision, parent)
                        .await
                        .map_err(unavailable)?
                        .into_iter()
                        .find(|entry| entry.path == path)
                        .ok_or_else(missing)?
                        .sha
                }
            } else if crate::backend::git::is_full_commit_id(expression) {
                expression.to_owned()
            } else {
                let refs = store.refs().await.map_err(unavailable)?;
                if let Some(oid) = refs
                    .get(expression)
                    .or_else(|| refs.get(&format!("refs/heads/{expression}")))
                    .or_else(|| refs.get(&format!("refs/tags/{expression}")))
                {
                    oid.clone()
                } else {
                    store.commit(expression).await.map_err(unavailable)?.sha
                }
            };
            load(repo, &oid, request).await
        }
        "refs" => {
            super::connection::validate(args)?;
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
            let refs = store.refs().await.map_err(unavailable)?;
            let mut names = refs.keys().cloned()
                .filter(|name| name.starts_with(&prefix) && name[prefix.len()..].contains(query))
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
            let page = super::connection::Page::new(args, names.len() as i64)?;
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
    repo: &db::models::Repository,
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
