//! Checks and legacy statuses share a rollup, while their credential scopes remain distinct.

use crate::arguments::Arguments;
use crate::db;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use codeyard_api_github::node_id;
use serde_json::json;
use serde_json::Value;

fn suite_object(repo: i64, id: i64) -> Value {
    json!({"__typename":"CheckSuite","id":node_id::encode("CSU",id),"_repo":repo,"_scope":"checks","_suite":id})
}
fn run_object(
    run: &db::checks::Run,
    oid: &str,
    request: &RequestContext,
    output: bool,
) -> Result<Value> {
    request.charge(
        serde_json::to_vec(&run.data.0).map_err(unavailable)?.len() + run.name.len() + 256,
    )?;
    let mut value = json!({"__typename":"CheckRun","id":node_id::encode("CRU",run.id),"databaseId":run.id,"_repo":run.repo_id,"_scope":"checks","_oid":oid,"_run":run.id,"_suite":run.suite_id,
        "name":run.name,"status":run.status.to_uppercase(),"conclusion":run.conclusion.as_ref().map(|value| value.to_uppercase()),"startedAt":run.data.started_at,"completedAt":run.data.completed_at,"detailsUrl":run.data.details_url,"externalId":run.data.external_id});
    if output {
        value["summary"] = json!(run.data.summary);
        value["text"] = json!(run.data.text);
        value["title"] = json!(run.data.title);
    }
    Ok(value)
}

pub(super) async fn rollup(repo: i64, oid: &str, request: &RequestContext) -> Result<Value> {
    let viewer = request.private_viewer(repo, "statuses").await?;
    request.authorize(repo, "checks", false).await?;
    let statuses = db::repositories::protection::commit_status_page(
        &request.state.pool,
        viewer,
        repo,
        oid,
        0,
        0,
        false,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let checks = db::checks::rollup_counts(&request.state.pool, repo, oid)
        .await
        .map_err(unavailable)?;
    request.authorize(repo, "statuses", false).await?;
    request.authorize(repo, "checks", false).await?;
    if statuses.total == 0 && checks.is_empty() {
        return Ok(Value::Null);
    }
    let failed = statuses
        .counts
        .iter()
        .any(|(state, _)| matches!(state.as_str(), "failure" | "error"))
        || checks.iter().any(|(state, _)| {
            matches!(
                state.as_str(),
                "FAILURE"
                    | "TIMED_OUT"
                    | "ACTION_REQUIRED"
                    | "CANCELLED"
                    | "STALE"
                    | "STARTUP_FAILURE"
            )
        });
    let pending = statuses.counts.iter().any(|(state, _)| state == "pending")
        || checks
            .iter()
            .any(|(state, _)| !matches!(state.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED"));
    Ok(
        json!({"__typename":"StatusCheckRollup","id":node_id::git("SCR",&format!("{repo}/{oid}")),"_repo":repo,"_scope":"checks","_oid":oid,
        "_statusCounts":statuses.counts,"_checkCounts":checks,"state":if failed {"FAILURE"} else if pending {"PENDING"} else {"SUCCESS"}}),
    )
}

pub(super) async fn contexts(
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    crate::connection::validate(args)?;
    let repo = parent["_repo"].as_i64().ok_or_else(missing)?;
    let oid = parent["_oid"].as_str().ok_or_else(missing)?;
    let viewer = request.private_viewer(repo, "statuses").await?;
    request.authorize(repo, "checks", false).await?;
    let statuses: Vec<(String, i64)> =
        serde_json::from_value(parent["_statusCounts"].clone()).map_err(unavailable)?;
    let checks: Vec<(String, i64)> =
        serde_json::from_value(parent["_checkCounts"].clone()).map_err(unavailable)?;
    let status_total: i64 = statuses.iter().map(|(_, count)| count).sum();
    let check_total: i64 = checks.iter().map(|(_, count)| count).sum();
    let page = crate::connection::Page::new(args, status_total.saturating_add(check_total))?;
    let status_limit = page
        .limit
        .min(status_total.saturating_sub(page.offset).max(0));
    let mut nodes = Vec::new();
    if status_limit > 0 {
        let rows = db::repositories::protection::commit_status_page(
            &request.state.pool,
            viewer,
            repo,
            oid,
            page.offset,
            status_limit,
            false,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        for row in rows.statuses {
            nodes.push(crate::git::status(repo, &row));
        }
    }
    let check_limit = page.limit - status_limit;
    if check_limit > 0 {
        let rows = db::checks::rollup_runs(
            &request.state.pool,
            repo,
            oid,
            page.offset.saturating_sub(status_total).max(0),
            check_limit,
        )
        .await
        .map_err(unavailable)?;
        for row in rows {
            nodes.push(run_object(&row, oid, request, false)?);
        }
    }
    request.authorize(repo, "statuses", false).await?;
    request.authorize(repo, "checks", false).await?;
    let mut value = page.finish(nodes);
    value["statusContextCount"] = json!(status_total);
    value["checkRunCount"] = json!(check_total);
    value["statusContextCountsByState"] = json!(statuses
        .into_iter()
        .map(|(state, count)| json!({"state":state.to_uppercase(),"count":count}))
        .collect::<Vec<_>>());
    value["checkRunCountsByState"] = json!(checks
        .into_iter()
        .map(|(state, count)| json!({"state":state,"count":count}))
        .collect::<Vec<_>>());
    Ok(value)
}

pub(super) async fn node(kind: &str, id: i64, request: &RequestContext) -> Result<Value> {
    let pool = &request.state.pool;
    let repo = db::graphql::node_repository(pool, kind, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    crate::repository::by_id(repo, request).await?;
    request.authorize(repo, "checks", false).await?;
    let value = if kind == "CSU" {
        db::checks::suite(pool, repo, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        suite_object(repo, id)
    } else {
        let run = db::checks::run(pool, repo, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let suite = db::checks::suite(pool, repo, run.suite_id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        run_object(&run, &suite.head_sha, request, true)?
    };
    request.authorize(repo, "checks", false).await?;
    Ok(value)
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let repo = parent["_repo"].as_i64().ok_or_else(missing)?;
    match (ty, field) {
        ("CheckRun", "isRequired") => crate::git::resolve(ty, field, args, request, parent).await,
        ("CheckRun", "checkSuite") => Ok(suite_object(
            repo,
            parent["_suite"].as_i64().ok_or_else(missing)?,
        )),
        ("CheckRun", "summary" | "text" | "title") => {
            let row = db::checks::run(
                &request.state.pool,
                repo,
                parent["_run"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
            request.charge(serde_json::to_vec(&row.data.0).map_err(unavailable)?.len())?;
            request.authorize(repo, "checks", false).await?;
            Ok(json!(match field {
                "summary" => row.data.summary.clone(),
                "text" => row.data.text.clone(),
                _ => row.data.title.clone(),
            }))
        }
        ("CheckSuite", "workflowRun") => {
            if !crate::repository::optional_authority(repo, "actions", request).await? {
                return Ok(Value::Null);
            }
            let row = db::checks::workflow_metadata(
                &request.state.pool,
                repo,
                parent["_suite"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(unavailable)?;
            request.authorize(repo, "actions", false).await?;
            Ok(row.map_or(Value::Null, |(event,path,name)| json!({"__typename":"WorkflowRun","_repo":repo,"_scope":"actions","event":event,"workflow":{"__typename":"Workflow","_repo":repo,"_scope":"actions","name":name.unwrap_or_else(|| codeyard_app::ci::workflows::workflow_name_from_path(&path))}})))
        }
        _ => Err(codeyard_model::graphql_error::error(
            format!("Codeyard does not support {ty}.{field} yet"),
            "UNPROCESSABLE",
        )),
    }
}
