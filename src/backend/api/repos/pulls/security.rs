//! Code scanning checks are projections of accepted analyses, never fabricated CI runs.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::discussion::unavailable;
use crate::backend::api::repos::discussion::RepositoryData;
use crate::backend::api::repos::discussion::ResultPage;

use codeyard_db_core::models::PullRequest;
use codeyard_webui::{encode_component, excerpt, pages::repos::security as view};
use serde_json::Value;

pub(super) async fn checks(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &PullRequest,
    head: &str,
    base: &str,
) -> ResultPage<Vec<view::Check>> {
    let Some(user) = &ctx.user else {
        return Ok(Vec::new());
    };
    let Some(session) = crate::backend::api::http::session::extract_session_id(&ctx.headers) else {
        return Ok(Vec::new());
    };
    let pool = &ctx.state.pool;
    let branch = (pull.head_repo_id == Some(repo.record.id))
        .then(|| format!("refs/heads/{}", pull.head_branch));
    let candidates = crate::backend::db::code_scanning::pull_merge_candidates(
        pool,
        user.id,
        &session,
        repo.record.id,
        pull.number,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let mut more = candidates.len() > 100;
    let store =
        crate::backend::repositories::store::repo_store(&repo.record, ctx.state.cluster.as_ref());
    let mut merges = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    for candidate in candidates.into_iter().take(100) {
        let result = match tokio::time::timeout_at(deadline, store.commit(&candidate)).await {
            Ok(result) => result,
            Err(_) => {
                more = true;
                break;
            }
        };
        match result {
            Ok(commit)
                if commit.parents.len() == 2
                    && commit.parents[0] == base
                    && commit.parents[1] == head =>
            {
                merges.push(commit.sha)
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind,
                    codeyard_git::error::ErrorKind::NotFound
                        | codeyard_git::error::ErrorKind::Invalid
                ) => {}
            Err(_) => more = true,
        }
    }
    let baseline_ref = format!("refs/heads/{}", pull.base_branch);
    let snapshot = crate::backend::db::code_scanning::pull_snapshot(
        pool,
        user.id,
        &session,
        &crate::backend::db::code_scanning::PullSelection {
            repo: repo.record.id,
            head,
            base,
            number: pull.number,
            branch: branch.as_deref(),
            base_branch: &baseline_ref,
            merge_commits: &merges,
        },
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    more |= snapshot.more;
    let mut checks = Vec::new();
    let root = format!("{}/security/code-scanning", repo.view.url());
    for group in snapshot.groups {
        let analysis = group.analysis;
        let tool: Value = serde_json::from_str(&analysis.tool).map_err(|_| unavailable(ctx))?;
        let tool = tool["name"].as_str().unwrap_or("Scanner");
        let baseline = group.baseline;
        let total = group.total;
        let mut annotations = Vec::new();
        for finding in group.findings {
            let rule: Value = serde_json::from_str(&finding.rule).map_err(|_| unavailable(ctx))?;
            let location: Value =
                serde_json::from_str(&finding.location).map_err(|_| unavailable(ctx))?;
            let path = location["path"].as_str().unwrap_or("");
            if path.len() > 4096 {
                continue;
            }
            let suffix = if analysis.ref_name.len() <= 1024 {
                format!("?branch={}", encode_component(&analysis.ref_name))
            } else {
                String::new()
            };
            annotations.push(view::Annotation {
                number: finding.number,
                rule: excerpt(rule["id"].as_str().unwrap_or("Finding"), 256),
                message: excerpt(&finding.message, 1024),
                path: path.to_owned(),
                line: location["start_line"].as_i64().unwrap_or(1),
                url: format!("{root}/{}{suffix}", finding.number),
            });
        }
        let url = if analysis.ref_name.len() <= 1024 && tool.len() <= 1024 {
            format!(
                "{root}?branch={}&tool={}",
                encode_component(&analysis.ref_name),
                encode_component(tool)
            )
        } else {
            root.clone()
        };
        checks.push(view::Check {
            name:if analysis.category.is_empty() {excerpt(tool,128)} else {format!("{} / {}",excerpt(tool,128),excerpt(&analysis.category,128))},
            state:if baseline.is_none() {"neutral"} else if total>0 {"failure"} else if group.incomplete {"neutral"} else {"success"}.into(),
            description:if group.incomplete {format!("{total} retained findings. Analysis results were truncated; this comparison is incomplete.")} else if baseline.is_none() {format!("{total} findings. No matching analysis exists for the base revision; these are not classified as newly introduced.")} else {format!("{total} new findings compared with the base revision.")},
            url, truncated:group.incomplete || total>annotations.len() as i64, annotations,
        });
    }
    if more {
        checks.push(view::Check {
            name: "More code scanning analyses".into(),
            state: "neutral".into(),
            description:
                "Some analysis groups or merge revisions could not be included. See Security for all analyses."
                    .into(),
            url: root.clone(),
            annotations: Vec::new(),
            truncated: false,
        });
    }
    for (reference, status, error) in snapshot.processing {
        if status != "complete" {
            checks.push(view::Check {
                name: format!("SARIF upload / {}", excerpt(&reference, 128)),
                state: if status == "pending" {
                    "pending"
                } else {
                    "failure"
                }
                .into(),
                description: excerpt(
                    error.as_deref().unwrap_or("SARIF processing is pending."),
                    512,
                ),
                url: root.clone(),
                annotations: Vec::new(),
                truncated: false,
            });
        }
    }
    Ok(checks)
}
