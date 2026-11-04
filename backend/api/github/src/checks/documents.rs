use crate::checks::run_value;
use codeyard_app::app::AppState;
use codeyard_db_actions::checks;
use codeyard_db_actions::checks::Run;
use codeyard_db_core::models::Repository;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::collections::HashMap;

pub(super) async fn runs(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    rows: &[Run],
) -> Result<Vec<Value>> {
    let ids: Vec<_> = rows.iter().map(|run| run.id).collect();
    let metadata: HashMap<_, _> = checks::run_details(&state.pool, repo.id, &ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|detail| (detail.run_id, detail))
        .collect();
    let (apps, heads) = keys(metadata.values().map(|detail| &detail.suite));
    let related = Related::load(&state.pool, repo, owner, &apps, &heads).await?;
    rows.iter()
        .map(|run| {
            let detail = metadata.get(&run.id).ok_or_else(Error::missing)?;
            Ok(run_value(
                repo,
                owner,
                run,
                &detail.suite,
                detail.annotation_count,
                related.publisher(detail.suite.app_id),
                related.pulls(&detail.suite.head_sha),
            ))
        })
        .collect()
}

use codeyard_repos_core::api_shared::github::checks::documents::Related;

use codeyard_repos_core::api_shared::github::checks::documents::keys;
