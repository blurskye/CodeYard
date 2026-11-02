//! Code scanning checks are projections of accepted analyses, never fabricated CI runs.
use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{unavailable, RepositoryData, ResultPage},
            security::excerpt,
        },
    },
    db::{code_scanning as records, models::PullRequest},
};
use crate::webui::{encode_component, pages::repos::security as view};
use serde_json::Value;

pub(super) async fn checks(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: &PullRequest,
    head: &str,
    base: &str,
) -> ResultPage<Vec<view::Check>> {
    // Alerts require write access even when the pull request itself is public.
    let allowed = match &ctx.user {
        Some(user) => {
            crate::backend::repositories::access::user_access(&ctx.state.pool, user, &repo.record)
                .await
                .map(|level| level.can_write())
        }
        None => Ok(false),
    }
    .map_err(|_| unavailable(ctx))?;
    if !allowed {
        return Ok(Vec::new());
    }
    let pool = &ctx.state.pool;
    let branch = (pull.head_repo_id == Some(repo.record.id))
        .then(|| format!("refs/heads/{}", pull.head_branch));
    let analyses =
        records::pull_analyses(pool, repo.record.id, head, pull.number, branch.as_deref())
            .await
            .map_err(|_| unavailable(ctx))?;
    let mut checks = Vec::new();
    let mut remaining = 100;
    let root = format!("{}/security/code-scanning", repo.view.url());
    for analysis in analyses.iter().take(100) {
        let tool: Value = serde_json::from_str(&analysis.tool).map_err(|_| unavailable(ctx))?;
        let tool = tool["name"].as_str().unwrap_or("Scanner");
        let baseline = records::baseline_analysis(
            pool,
            repo.record.id,
            &analysis.tool_key,
            &analysis.category,
            &format!("refs/heads/{}", pull.base_branch),
            base,
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        let findings = records::new_findings(pool, analysis.id, baseline)
            .await
            .map_err(|_| unavailable(ctx))?;
        let total = findings.first().map_or(0, |finding| finding.total_count);
        let mut annotations = Vec::new();
        for finding in findings.into_iter().take(remaining) {
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
        remaining -= annotations.len();
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
            state:if baseline.is_none() {"neutral"} else if total>0 {"failure"} else {"success"}.into(),
            description:if baseline.is_none() {format!("{total} findings. No matching analysis exists for the base revision; these are not classified as newly introduced.")} else {format!("{total} new findings compared with the base revision.")},
            url, truncated:total>annotations.len() as i64, annotations,
        });
    }
    if analyses.len() > 100 {
        checks.push(view::Check {
            name: "More code scanning analyses".into(),
            state: "neutral".into(),
            description:
                "Only the latest 100 analysis groups are shown. See Security for all analyses."
                    .into(),
            url: root.clone(),
            annotations: Vec::new(),
            truncated: false,
        });
    }
    if let Some((status, error)) =
        records::pull_processing(pool, repo.record.id, head, pull.number, branch.as_deref())
            .await
            .map_err(|_| unavailable(ctx))?
    {
        if status != "complete" {
            checks.push(view::Check {
                name: "SARIF upload".into(),
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
                url: root,
                annotations: Vec::new(),
                truncated: false,
            });
        }
    }
    Ok(checks)
}
