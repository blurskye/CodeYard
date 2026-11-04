//! Branch creation uses the normal ref publication checks before recording the link.
use super::{db, OperationError};
use crate::backend::{
    app::AppState,
    repositories::{access, refs, store::repo_store},
};
use codeyard_git::error::ErrorKind;

pub fn default_name(issue: &codeyard_db_core::models::Issue) -> String {
    let title = issue
        .title
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let mut name = format!(
        "{}-{}",
        issue.number,
        title
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-")
    );
    // Git keeps the branch in one file, and in `<name>.lock` while it changes, so the name must
    // fit a 255-byte file name. The name is ASCII, so any length is a character boundary.
    name.truncate(250);
    name.truncate(name.trim_end_matches('-').len());
    name
}

pub async fn create(
    state: &AppState,
    actor: impl Into<db::credentials::Actor>,
    issue: &codeyard_db_core::models::Issue,
    target: &codeyard_db_core::models::Repository,
    name: &str,
    oid: &str,
) -> Result<db::issues::management::LinkedBranch, OperationError> {
    let authority = actor.into();
    access::check_write(&state.pool, issue.repo_id, authority.user_id)
        .await
        .map_err(|_| OperationError::Denied {
            code: "forbidden",
            message: "Write access is required".into(),
        })?;
    let owner = db::repositories::records::repository_owner_name(&state.pool, target.id)
        .await
        .map_err(super::database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    let created = refs::create_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        authority.clone(),
        &owner,
        target.id,
        name,
        oid,
    )
    .await;
    let unconfirmed = || OperationError::Unavailable {
        code: "branch_error",
        message: "Branch creation was not confirmed; inspect the repository before retrying".into(),
    };
    match created {
        // The branch exists; only refreshing open pull requests after it failed, so the
        // issue still gets its link.
        Ok(_) | Err(refs::RefError::AppliedFollowupFailed) => {}
        Err(refs::RefError::Invalid(message) | refs::RefError::Protected(message)) => {
            return Err(OperationError::Invalid {
                code: "invalid_branch",
                message: message.into(),
            })
        }
        Err(refs::RefError::Access(_)) => {
            return Err(OperationError::Denied {
                code: "forbidden",
                message: "Write access is required".into(),
            })
        }
        Err(refs::RefError::Git(error)) => match error.kind {
            ErrorKind::Invalid | ErrorKind::NotFound | ErrorKind::MissingPath => {
                return Err(OperationError::Invalid {
                    code: "invalid_branch",
                    message: "The branch name or the commit it starts from is not valid".into(),
                })
            }
            // A retry after the branch was published but its link was not saved finds the
            // branch at the requested commit; it then only saves the link.
            ErrorKind::Conflict => {
                let reference = codeyard_git::branch_ref(name).ok_or_else(unconfirmed)?;
                let tip = repo_store(target, state.cluster.as_ref())
                    .commit_from_primary(&reference)
                    .await
                    .map_err(|_| unconfirmed())?;
                if !tip.sha.eq_ignore_ascii_case(oid) {
                    return Err(OperationError::Conflict {
                        code: "branch_exists",
                        message: "A branch with this name already exists".into(),
                    });
                }
            }
            ErrorKind::Unavailable => return Err(unconfirmed()),
        },
        Err(refs::RefError::Unavailable) => return Err(unconfirmed()),
    }
    db::issues::management::link_branch(
        &state.pool,
        authority,
        issue.repo_id,
        issue.id,
        target.id,
        name,
    )
    .await
    .map_err(|_| OperationError::Unavailable {
        code: "branch_link_error",
        message: "Branch created, but its issue link was not confirmed".into(),
    })
}
