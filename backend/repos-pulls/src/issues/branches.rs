//! Branch creation uses the normal ref publication checks before recording the link.
use crate::issues::OperationError;
use crate::services::RepositoryServices;
use codeyard_git::error::ErrorKind;
use codeyard_git_storage::repositories::store::repo_store;

pub fn default_name(issue: &codeyard_database::models::Issue) -> String {
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
    state: &dyn RepositoryServices,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    issue: &codeyard_database::models::Issue,
    target: &codeyard_database::models::Repository,
    name: &str,
    oid: &str,
) -> Result<codeyard_db_repos::issues::management::LinkedBranch, OperationError> {
    let authority = actor.into();
    codeyard_identity::repository_access::check_write(
        state.pool(),
        issue.repo_id,
        authority.user_id,
    )
    .await
    .map_err(|_| OperationError::Denied {
        code: "forbidden",
        message: "Write access is required".into(),
    })?;
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(state.pool(), target.id)
            .await
            .map_err(crate::issues::database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Repository not found".into(),
            })?;
    let created = codeyard_ci_engine::refs::create_branch(
        state.pool(),
        state.cluster(),
        state.ci_runs(),
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
        Ok(_) | Err(codeyard_git_storage::ref_errors::RefError::AppliedFollowupFailed) => {}
        Err(
            codeyard_git_storage::ref_errors::RefError::Invalid(message)
            | codeyard_git_storage::ref_errors::RefError::Protected(message),
        ) => {
            return Err(OperationError::Invalid {
                code: "invalid_branch",
                message: message.into(),
            })
        }
        Err(codeyard_git_storage::ref_errors::RefError::Access(_)) => {
            return Err(OperationError::Denied {
                code: "forbidden",
                message: "Write access is required".into(),
            })
        }
        Err(codeyard_git_storage::ref_errors::RefError::Git(error)) => match error.kind {
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
                let tip = repo_store(target, state.cluster())
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
        Err(codeyard_git_storage::ref_errors::RefError::Unavailable) => return Err(unconfirmed()),
    }
    codeyard_db_repos::issues::management::link_branch(
        state.pool(),
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
