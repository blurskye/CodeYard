//! Repository-list selections use the same Git files and catalogues as native pages.
use super::{
    connection, db, missing, repository, unavailable, url, user, Arguments, RequestContext, Result,
    Value,
};
use crate::backend::repositories::{
    issue_templates,
    store::{repo_store, RepoStore},
    templates,
};
use async_graphql::Error;
use serde_json::json;

async fn texts(store: &RepoStore, branch: &str, paths: &[&str]) -> Result<Vec<(String, String)>> {
    let refs = store.refs().await.map_err(unavailable)?;
    let Some(revision) = refs.get(&format!("refs/heads/{branch}")) else {
        return Ok(Vec::new());
    };
    let paths: Vec<_> = paths.iter().map(|p| (*p).to_owned()).collect();
    Ok(paths
        .iter()
        .cloned()
        .zip(
            store
                .template_texts(revision, &paths)
                .await
                .map_err(unavailable)?,
        )
        .filter_map(|(path, text)| text.map(|(_, text)| (path, text)))
        .collect())
}

fn yaml(text: &str) -> Result<serde_yaml::Value> {
    crate::backend::ci::workflows::parse_yaml_with_limit(
        text.as_bytes(),
        4 * issue_templates::MAX_TEMPLATE_BYTES as usize,
    )
    .map_err(unavailable)
}

pub(super) async fn resolve(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    repo: &db::models::Repository,
) -> Result<Value> {
    let pool = &request.state.pool;
    let actor = &request.auth.user;
    let store = repo_store(repo, request.state.cluster.as_ref());
    match field {
        "usesCustomOpenGraphImage" | "isMirror" => Ok(json!(false)),
        // These optional facts are not recorded by Codeyard.
        "mirrorUrl" | "templateRepository" | "archivedAt" => Ok(Value::Null),
        "openGraphImageUrl" => Ok(json!(url("/assets/favicon.png"))),
        "diskUsage" => Ok(json!(
            i32::try_from(repo.size_bytes / 1024).map_err(unavailable)?
        )),
        "isEmpty" => Ok(json!(store.refs().await.map_err(unavailable)?.is_empty())),
        "isUserConfigurationRepository" => Ok(json!(repo.name == ".github")),
        "viewerDefaultCommitEmail" => Ok(json!(actor.commit_email())),
        "viewerPossibleCommitEmails" => {
            let mut emails: Vec<_> = db::users::emails::list_emails(pool, actor.id)
                .await
                .map_err(unavailable)?
                .into_iter()
                .filter(|email| email.verified)
                .map(|email| email.email)
                .collect();
            emails.push(actor.noreply_email());
            emails.sort();
            emails.dedup();
            Ok(json!(emails))
        }
        "viewerDefaultMergeMethod" => Ok(json!(if repo.allow_merge_commit {
            "MERGE"
        } else if repo.allow_squash_merge {
            "SQUASH"
        } else {
            "REBASE"
        })),
        "viewerHasStarred" => Ok(json!(db::repositories::engagement::is_repo_starred(
            pool, actor.id, repo.id
        )
        .await
        .map_err(unavailable)?)),
        "viewerSubscription" => {
            let level = db::repositories::engagement::get_watch_level(pool, actor.id, repo.id)
                .await
                .map_err(unavailable)?;
            Ok(json!(
                match level.as_ref().map(|(level, _)| level.as_str()) {
                    Some("ignoring") => "IGNORED",
                    Some("watching" | "custom") => "SUBSCRIBED",
                    _ => "UNSUBSCRIBED",
                }
            ))
        }
        "isBlankIssuesEnabled" | "issueTemplates" => {
            let files = issue_templates::issue_templates(&store, &repo.default_branch)
                .await
                .map_err(unavailable)?;
            Ok(if field == "isBlankIssuesEnabled" {
                json!(files.blank_allowed)
            } else {
                json!(files
                    .templates
                    .iter()
                    .filter(|t| t.form.is_empty())
                    .map(|t| json!({"name":t.name,"title":t.title,"body":t.body,"about":t.about}))
                    .collect::<Vec<_>>())
            })
        }
        "pullRequestTemplates" => {
            let files = texts(
                &store,
                &repo.default_branch,
                &issue_templates::PULL_REQUEST_TEMPLATE_PATHS,
            )
            .await?;
            Ok(json!(files
                .into_iter()
                .take(1)
                .map(|(path, body)| json!({"body":body,"filename":path}))
                .collect::<Vec<_>>()))
        }
        "securityPolicyUrl" | "isSecurityPolicyEnabled" => {
            let files = texts(
                &store,
                &repo.default_branch,
                &[".github/SECURITY.md", "SECURITY.md", "docs/SECURITY.md"],
            )
            .await?;
            if field == "isSecurityPolicyEnabled" {
                return Ok(json!(!files.is_empty()));
            }
            let owner = repository::owner(repo, request).await?;
            Ok(json!(files.first().map(|(path, _)| url(&format!(
                "/{}/{}/blob/{}/{path}",
                owner, repo.name, repo.default_branch
            )))))
        }
        "contactLinks" => {
            let files = texts(
                &store,
                &repo.default_branch,
                &[".github/ISSUE_TEMPLATE/config.yml"],
            )
            .await?;
            let mut links = Vec::new();
            if let Some((_, text)) = files.first() {
                let config = yaml(text)?;
                for link in config
                    .get("contact_links")
                    .and_then(serde_yaml::Value::as_sequence)
                    .into_iter()
                    .flatten()
                {
                    if let (Some(name), Some(about), Some(url)) = (
                        link.get("name").and_then(serde_yaml::Value::as_str),
                        link.get("about").and_then(serde_yaml::Value::as_str),
                        link.get("url").and_then(serde_yaml::Value::as_str),
                    ) {
                        links.push(json!({"name":name,"about":about,"url":url}));
                    }
                }
            }
            Ok(json!(links))
        }
        "fundingLinks" => funding(&store, &repo.default_branch).await,
        "codeOfConduct" => {
            let files = texts(
                &store,
                &repo.default_branch,
                &[
                    "CODE_OF_CONDUCT.md",
                    ".github/CODE_OF_CONDUCT.md",
                    "docs/CODE_OF_CONDUCT.md",
                ],
            )
            .await?;
            let owner = repository::owner(repo, request).await?;
            Ok(files.first().map(|(path, text)| json!({"key":if text.contains("Contributor Covenant") { "contributor_covenant" } else { "other" },"name":if text.contains("Contributor Covenant") { "Contributor Covenant" } else { "Other" },"url":crate::webui::pages::repos::ref_path::branch_url(&url(&format!("/{}/{}",owner,repo.name)),"blob",&repo.default_branch,path)})).unwrap_or(Value::Null))
        }
        "licenseInfo" => {
            let files = texts(
                &store,
                &repo.default_branch,
                &[
                    "LICENSE",
                    "LICENSE.md",
                    "LICENSE.txt",
                    "LICENCE",
                    "LICENCE.md",
                    "LICENCE.txt",
                    "COPYING",
                    "COPYING.md",
                    "COPYING.txt",
                ],
            )
            .await?;
            Ok(files.first().map(|(_, text)| {
                templates::detect_license(text.as_bytes()).map(|template| json!({"key":template.name.to_lowercase(),"name":template.name,"nickname":Value::Null})).unwrap_or(json!({"key":"other","name":"Other","nickname":Value::Null}))
            }).unwrap_or(Value::Null))
        }
        "languages" | "primaryLanguage" => {
            let mut languages = if store
                .refs()
                .await
                .map_err(unavailable)?
                .contains_key(&format!("refs/heads/{}", repo.default_branch))
            {
                store
                    .languages(&repo.default_branch)
                    .await
                    .map_err(unavailable)?
            } else {
                Vec::new()
            };
            languages.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.language.cmp(&b.language)));
            if field == "primaryLanguage" {
                return Ok(languages
                    .first()
                    .map(|l| json!({"name":l.language,"color":l.color}))
                    .unwrap_or(Value::Null));
            }
            if let Some(order) = ctx.args.get("orderBy").filter(|v| !v.is_null()) {
                let order = order.object()?;
                match order.try_get("field")?.enum_name()? {
                    "SIZE" => {}
                    "ALPHABETICAL" => languages.sort_by(|a, b| b.language.cmp(&a.language)),
                    _ => return Err(Error::new("Unsupported language order")),
                }
                if order.try_get("direction")?.enum_name()? == "ASC" {
                    languages.reverse();
                }
            }
            let total = languages.iter().map(|l| l.bytes).sum::<u64>();
            let page = connection::Page::new(ctx, languages.len() as i64)?;
            let sizes: Vec<_> = languages
                .iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
                .map(|l| l.bytes)
                .collect();
            let nodes = languages
                .iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
                .map(|l| json!({"name":l.language,"color":l.color}))
                .collect();
            let mut result = page.finish(nodes);
            for (edge, size) in result["edges"]
                .as_array_mut()
                .ok_or_else(missing)?
                .iter_mut()
                .zip(sizes)
            {
                edge["size"] = json!(size);
            }
            result["totalSize"] = json!(total);
            Ok(result)
        }
        "assignableUsers" | "mentionableUsers" => {
            let names = db::assignees::list_assignable_usernames(pool, repo.id)
                .await
                .map_err(unavailable)?;
            let filter = ctx
                .args
                .get("query")
                .filter(|v| !v.is_null())
                .map(|v| v.string())
                .transpose()?
                .unwrap_or("")
                .to_lowercase();
            let mut nodes = Vec::new();
            for name in names
                .into_iter()
                .filter(|name| name.to_lowercase().contains(&filter))
            {
                let account = db::users::accounts::get_user_by_username(pool, &name)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                nodes.push(user::by_id(account.id, request).await?);
            }
            connection::slice(ctx, nodes)
        }
        // Codeyard stores Projects v2, not retired classic projects.
        _ => Err(Error::new(format!(
            "Codeyard does not support Repository.{field} yet"
        ))),
    }
}

async fn funding(store: &RepoStore, branch: &str) -> Result<Value> {
    let files = texts(store, branch, &[".github/FUNDING.yml"]).await?;
    let Some((_, text)) = files.first() else {
        return Ok(json!([]));
    };
    let config = yaml(text)?;
    let mut links = Vec::new();
    for (key, platform, prefix) in [
        ("github", "GITHUB", "https://github.com/sponsors/"),
        ("patreon", "PATREON", "https://www.patreon.com/"),
        (
            "open_collective",
            "OPEN_COLLECTIVE",
            "https://opencollective.com/",
        ),
        ("ko_fi", "KO_FI", "https://ko-fi.com/"),
        (
            "tidelift",
            "TIDELIFT",
            "https://tidelift.com/funding/github/",
        ),
        (
            "community_bridge",
            "COMMUNITY_BRIDGE",
            "https://funding.communitybridge.org/projects/",
        ),
        ("liberapay", "LIBERAPAY", "https://liberapay.com/"),
        ("issuehunt", "ISSUEHUNT", "https://issuehunt.io/r/"),
        (
            "lfx_crowdfunding",
            "LFX_CROWDFUNDING",
            "https://crowdfunding.lfx.linuxfoundation.org/projects/",
        ),
        ("polar", "POLAR", "https://polar.sh/"),
        (
            "buy_me_a_coffee",
            "BUY_ME_A_COFFEE",
            "https://www.buymeacoffee.com/",
        ),
        ("thanks_dev", "THANKS_DEV", "https://thanks.dev/"),
        ("custom", "CUSTOM", ""),
    ] {
        if let Some(value) = config.get(key) {
            let values = value
                .as_sequence()
                .map(|v| v.iter().collect::<Vec<_>>())
                .unwrap_or_else(|| vec![value]);
            for value in values {
                if let Some(value) = value.as_str().filter(|s| !s.is_empty()) {
                    links.push(json!({"platform":platform,"url":format!("{prefix}{value}")}));
                }
            }
        }
    }
    Ok(json!(links))
}
