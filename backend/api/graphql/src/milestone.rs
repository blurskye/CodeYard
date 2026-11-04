use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use serde_json::Value;

pub(super) async fn resolve(
    field: &str,
    args: &Arguments,
    repo: &codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<Value> {
    request.authorize(repo.id, "issues", false).await?;
    let pool = &request.state.pool;
    if field == "milestone" {
        return match codeyard_db_repos::milestones::get_milestone(
            pool,
            repo.id,
            args.args.try_get("number")?.i64()? as i32,
        )
        .await
        .map_err(unavailable)?
        {
            Some(row) => crate::collaboration::milestone(row, request).await,
            None => Ok(Value::Null),
        };
    }
    crate::connection::validate(args)?;
    let mut states = args
        .args
        .get("states")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<Vec<String>>())
        .transpose()?
        .unwrap_or_default();
    states.sort();
    states.dedup();
    let state = if states.len() == 1 {
        Some(states[0].to_ascii_lowercase())
    } else {
        None
    };
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
    let order = match (
        order
            .as_ref()
            .and_then(|v| v["field"].as_str())
            .unwrap_or("DUE_DATE"),
        order
            .as_ref()
            .and_then(|v| v["direction"].as_str())
            .unwrap_or("ASC"),
    ) {
        ("DUE_DATE", "ASC") => "due-on-asc",
        ("DUE_DATE", "DESC") => "due-on-desc",
        ("CREATED_AT", "ASC") => "created-asc",
        ("CREATED_AT", "DESC") => "created-desc",
        ("UPDATED_AT", "ASC") => "updated-asc",
        ("UPDATED_AT", "DESC") => "updated-desc",
        ("NUMBER", "ASC") => "number-asc",
        ("NUMBER", "DESC") => "number-desc",
        _ => return Err(missing()),
    };
    let page = crate::connection::Page::new(
        args,
        codeyard_db_repos::milestones::count_matching_milestones(
            pool,
            repo.id,
            state.as_deref(),
            query,
        )
        .await
        .map_err(unavailable)?,
    )?;
    let rows = codeyard_db_repos::milestones::search_milestones_page(
        pool,
        repo.id,
        state.as_deref(),
        query,
        order,
        page.limit,
        page.offset,
    )
    .await
    .map_err(unavailable)?;
    let mut nodes = vec![];
    for row in rows {
        nodes.push(crate::collaboration::milestone(row, request).await?);
    }
    Ok(page.finish(nodes))
}
