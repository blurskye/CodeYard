//! Public profile activity: pushes, issues, pull requests and comments, newest first.
use crate::backend::identity::activity::ActivityPage;
use codeyard_db_repos::activity::ActivityRow;
use codeyard_webui::pages::users::{ProfileActivity, ProfileActivityItem};
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct ActivityQuery {
    pub page: Option<i64>,
}

/// The presentation of one page of profile activity.
pub(crate) fn profile_activity_view(page: ActivityPage) -> ProfileActivity {
    ProfileActivity {
        items: page.items.into_iter().map(activity_item).collect(),
        next_page: page.next_page,
    }
}

fn activity_item(row: ActivityRow) -> ProfileActivityItem {
    let repository = format!("{}/{}", row.owner_name, row.repo_name);
    let repo_path = codeyard_webui::repository_path(&row.owner_name, &row.repo_name);
    let number = row.number.unwrap_or_default();
    let url = match row.kind.as_str() {
        "push" => format!(
            "{repo_path}/commits?ref={}",
            codeyard_webui::encode_component(&format!(
                "refs/heads/{}",
                row.branch.as_deref().unwrap_or_default()
            ))
        ),
        "issue_opened" | "issue_comment" => format!("{repo_path}/issues/{number}"),
        "pull_request_opened" | "pull_request_merged" | "pull_request_comment" => {
            format!("{repo_path}/pulls/{number}")
        }
        _ => repo_path,
    };
    ProfileActivityItem {
        kind: row.kind,
        created_at: row.created_at.to_rfc3339(),
        repository,
        url,
        number: row.number,
        title: row.title,
        branch: row.branch,
        commits: row.commit_count,
    }
}
