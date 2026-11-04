//! Discussions anchored to immutable commits, shared by REST and native forms.
use crate::cluster::context::NodeContext;
use crate::repositories::store::repo_store;
use crate::webhooks::payloads::Person;
use crate::webhooks::payloads::Scope;
use codeyard_db_core::models::CommitComment;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_db_repos::repositories::commit_comments::CommentCreate;
use codeyard_db_repos::repositories::commit_comments::CommentEdit;
use codeyard_db_repos::repositories::commit_comments::CommentRefusal;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_notifications::create::create_and_broadcast_notification;
use codeyard_notifications::create::NotificationRequest;
use codeyard_notifications::mentions::notify_mentions;
use codeyard_notifications::mentions::MentionComment;
use codeyard_notifications::mentions::MentionSource;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
pub enum CommentError {
    #[error("Commit comment not found")]
    Missing,
    #[error("Commit comment write denied")]
    Denied,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("The comment changed; your draft is preserved. Reload before saving.")]
    Conflict,
    #[error("Commit discussions are temporarily unavailable")]
    Unavailable,
}

type Result<T> = std::result::Result<T, CommentError>;

fn write_error(error: anyhow::Error) -> CommentError {
    match error.downcast_ref::<CommentRefusal>() {
        Some(CommentRefusal::Denied) => CommentError::Denied,
        Some(CommentRefusal::Invalid) => CommentError::Invalid("Invalid commit comment"),
        Some(CommentRefusal::Conflict) => CommentError::Conflict,
        None => CommentError::Unavailable,
    }
}

fn git_error(error: RepoError) -> CommentError {
    if error.kind == ErrorKind::NotFound {
        CommentError::Missing
    } else {
        CommentError::Unavailable
    }
}

#[derive(Deserialize)]
pub struct CreateComment {
    pub body: String,
    pub path: Option<String>,
    pub position: Option<i32>,
    pub line: Option<i32>,
    pub repository_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CommentDetails {
    pub comment: CommitComment,
    pub author: Option<Person>,
    pub association: String,
}

#[derive(Clone)]
struct Author {
    person: Person,
    association: String,
}

async fn authors(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repo: &Repository,
    ids: &[i64],
) -> Result<HashMap<i64, Author>> {
    let mut authors =
        codeyard_db_repos::repositories::commit_comments::comment_authors(pool, repo.id, ids)
            .await
            .map_err(|_| CommentError::Unavailable)?;
    if authors.iter().any(|author| author.association == "NONE") {
        // The contributor badge is optional, so a history too large to list keeps NONE.
        let contributors = match repo_store(repo, cluster).contributors().await {
            Ok(contributors) => contributors,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "commit comment contributors are unavailable");
                Vec::new()
            }
        };
        let emails: HashSet<_> = contributors
            .into_iter()
            .map(|contributor| contributor.email.to_ascii_lowercase())
            .collect();
        for author in &mut authors {
            // Commits made for the account carry its no-reply address or its own email.
            let noreply = format!("{}@users.noreply", author.username).to_ascii_lowercase();
            let committed = emails.contains(&noreply)
                || author
                    .email
                    .as_ref()
                    .is_some_and(|email| emails.contains(&email.to_ascii_lowercase()));
            if author.association == "NONE" && committed {
                author.association = "CONTRIBUTOR".into();
            }
        }
    }
    Ok(authors
        .into_iter()
        .map(|author| {
            (
                author.id,
                Author {
                    person: Person {
                        id: author.id,
                        login: author.username,
                        site_admin: author.is_admin,
                    },
                    association: author.association,
                },
            )
        })
        .collect())
}

/// Shared repository-discussion associations, including read collaborators and Git contributors.
/// The repository has already passed the caller's read check; no comment row is required.
pub async fn author_associations(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repo: &Repository,
    ids: &[i64],
) -> Result<HashMap<i64, String>> {
    Ok(authors(pool, cluster, repo, ids)
        .await?
        .into_iter()
        .map(|(id, author)| (id, author.association))
        .collect())
}

fn details(comment: CommitComment, authors: &HashMap<i64, Author>) -> CommentDetails {
    let author = comment.author_id.and_then(|id| authors.get(&id));
    CommentDetails {
        comment,
        author: author.map(|author| author.person.clone()),
        association: author.map_or_else(|| "NONE".into(), |author| author.association.clone()),
    }
}

/// The repository has already passed the caller's read check. Missing accounts stay null.
pub async fn comment_details(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repo: &Repository,
    comments: Vec<CommitComment>,
) -> Result<Vec<CommentDetails>> {
    let ids: HashSet<_> = comments
        .iter()
        .filter_map(|comment| comment.author_id)
        .collect();
    let authors = authors(pool, cluster, repo, &ids.into_iter().collect::<Vec<_>>()).await?;
    Ok(comments
        .into_iter()
        .map(|comment| details(comment, &authors))
        .collect())
}

async fn writable(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo: i64,
) -> Result<(User, Repository, String)> {
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| CommentError::Unavailable)?
        .ok_or(CommentError::Denied)?;
    if user.is_banned || user.must_change_password {
        return Err(CommentError::Denied);
    }
    let repository = codeyard_identity::repository_access::check_read(pool, repo, Some(actor))
        .await
        .map_err(|error| {
            if error == codeyard_identity::repository_access::AccessError::NotFound {
                CommentError::Missing
            } else {
                CommentError::Unavailable
            }
        })?;
    if repository.is_archived {
        return Err(CommentError::Denied);
    }
    let owner = codeyard_db_repos::repositories::records::repository_owner_name(pool, repo)
        .await
        .map_err(|_| CommentError::Unavailable)?
        .ok_or(CommentError::Unavailable)?;
    Ok((user, repository, owner))
}

pub fn comment_link(owner: &str, name: &str, comment: &CommitComment) -> String {
    format!(
        "/{}/{}/commit/{}?comment={}#commitcomment-{}",
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(name),
        comment.sha,
        comment.id,
        comment.id
    )
}

async fn emit(
    pool: &codeyard_db_core::DbPool,
    repo: &Repository,
    actor: i64,
    action: &'static str,
    entry: &CommentDetails,
) {
    let (repo_for_hook, entry) = (repo.clone(), entry.clone());
    crate::webhooks::emit(
        pool,
        repo,
        codeyard_model::webhook_events::COMMIT_COMMENT,
        vec![actor],
        move |owner, people| {
            let sender = people.get(actor);
            let comment = crate::webhooks::payloads::commit_comment(
                &entry.comment,
                entry.author.as_ref(),
                &entry.association,
                &repo_for_hook,
                owner,
                &[], // At creation the new comment has no reactions.
            );
            crate::webhooks::payloads::commit_comment_event(
                action,
                comment,
                Scope {
                    repo: &repo_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "A comment notice needs the storage, actor, repository and saved text snapshots."
)]
async fn notify(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    user: &User,
    repo: &Repository,
    owner: &str,
    comment: &CommitComment,
    previous: Option<&str>,
) {
    let link = comment_link(owner, &repo.name, comment);
    let title = format!(
        "{} commented on commit {}",
        user.username,
        codeyard_webui::components::short_sha(&comment.sha)
    );
    let mut notified = Vec::new();
    let mut subscriber_notice = None;
    // Creation notifies a known commit author who can still read the repository.
    if previous.is_none() {
        let recipient = async {
            let commit = repo_store(repo, cluster).commit(&comment.sha).await.ok()?;
            let author = crate::repositories::reads::commit_authors(
                pool,
                std::slice::from_ref(&commit.author_email),
            )
            .await
            .ok()?
            .remove(&commit.author_email)?;
            if author.id == user.id
                || author.is_banned
                || author.must_change_password
                || !codeyard_identity::repository_access::can_read(pool, Some(&author), repo)
                    .await
                    .ok()?
            {
                return None;
            }
            Some(author.id)
        }
        .await;
        let notice = NotificationRequest {
            user_id: user.id,
            actor_id: Some(user.id),
            repo_id: Some(repo.id),
            kind: "commit_comment",
            title: &title,
            body: Some(&comment.body),
            link: Some(&link),
            metadata: json!({"repo_owner":owner,"repo_name":repo.name,"commit_sha":comment.sha,"commit_comment_id":comment.id,"subject":comment.sha}),
        };
        if let Some(recipient) = recipient {
            let sent = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: recipient,
                    ..notice.clone()
                },
            )
            .await;
            if sent.is_ok() {
                notified.push(recipient);
            }
        }
        subscriber_notice = Some(notice);
    }
    // A mention comes before the subscriber notice, so a mentioned subscriber hears it as one.
    let mentioned = notify_mentions(
        pool,
        broadcaster,
        MentionSource {
            actor: user,
            repository: repo,
            owner,
            repo: &repo.name,
            number: 0,
            subject: &comment.sha,
            link: &link,
            comment_id: Some(MentionComment::Commit(comment.id)),
            old_text: previous,
            text: &comment.body,
            notified: &notified,
        },
    )
    .await;
    // Thread subscribers hear about every new comment on the commit.
    if let Some(notice) = subscriber_notice {
        notified.extend(mentioned);
        codeyard_notifications::create::notify_subscribers(pool, broadcaster, notice, &notified)
            .await;
    }
}

pub async fn create(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    reference: &str,
    mut input: CreateComment,
) -> Result<CommentDetails> {
    let credential = actor.into();
    let actor = credential.user_id;
    let (user, repository, owner) = writable(pool, actor, repo).await?;
    if input.repository_id.is_some_and(|id| id != repo) {
        return Err(CommentError::Conflict);
    }
    codeyard_db_repos::repositories::commit_comments::validate_body(&input.body).map_err(|_| {
        CommentError::Invalid("A comment needs a nonempty body of at most 65,536 characters")
    })?;
    let store = repo_store(&repository, cluster);
    let commit = store
        .commit_from_primary(reference)
        .await
        .map_err(git_error)?;
    if input.position.is_some_and(|position| position <= 0)
        || input.line.is_some_and(|line| line <= 0)
        || (input.path.is_none() && (input.position.is_some() || input.line.is_some()))
    {
        return Err(CommentError::Invalid(
            "A positive diff position or line needs a file path",
        ));
    }
    if let Some(path) = input.path.as_deref() {
        if path.len() > 4096 || path.contains('\0') {
            return Err(CommentError::Invalid("Invalid file path"));
        }
        let diff = match commit.parents.first() {
            Some(parent) => store.diff_file(parent, &commit.sha, path).await,
            None => store.commit_diff(&commit.sha).await,
        }
        .map_err(git_error)?;
        let file =
            diff.files
                .iter()
                .find(|file| file.filename == path)
                .ok_or(CommentError::Invalid(
                    "The commit does not change this file",
                ))?;
        if input.position.is_some() || input.line.is_some() {
            let rows = codeyard_webui::components::patch_rows(&file.patch);
            let start = rows
                .iter()
                .position(|row| row.text.starts_with("@@"))
                .ok_or(CommentError::Invalid(
                    "This file has no commentable diff lines",
                ))?
                + 1;
            let lines = &rows[start..];
            let position = match input.position {
                Some(position) => (position - 1) as usize,
                None => lines
                    .iter()
                    .position(|row| {
                        row.new.or(row.old).map(|line| line as i64) == input.line.map(i64::from)
                    })
                    .ok_or(CommentError::Invalid("Line is outside the commit diff"))?,
            };
            let line = lines
                .get(position)
                .and_then(|row| row.new.or(row.old))
                .ok_or(CommentError::Invalid("Position is outside the commit diff"))?;
            input.position = Some(
                i32::try_from(position + 1)
                    .map_err(|_| CommentError::Invalid("Diff position exceeds its limit"))?,
            );
            input.line = Some(
                i32::try_from(line)
                    .map_err(|_| CommentError::Invalid("Diff line exceeds its limit"))?,
            );
        }
    }
    let authors = authors(pool, cluster, &repository, &[actor]).await?;
    let comment = codeyard_db_repos::repositories::commit_comments::create_comment(
        pool,
        CommentCreate {
            actor: credential,
            repo_id: repo,
            sha: &commit.sha,
            body: &input.body,
            path: input.path.as_deref(),
            position: input.position,
            line: input.line,
        },
    )
    .await
    .map_err(write_error)?;
    let entry = details(comment, &authors);
    emit(pool, &repository, actor, "created", &entry).await;
    notify(
        pool,
        cluster,
        broadcaster,
        &user,
        &repository,
        &owner,
        &entry.comment,
        None,
    )
    .await;
    Ok(entry)
}

pub async fn edit(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    input: CommentEdit<'_>,
) -> Result<CommentDetails> {
    let (user, repository, owner) = writable(pool, input.actor.user_id, input.repo_id).await?;
    let previous = codeyard_db_repos::repositories::commit_comments::get_comment(
        pool,
        input.repo_id,
        input.id,
    )
    .await
    .map_err(|_| CommentError::Unavailable)?
    .ok_or(CommentError::Missing)?;
    if previous.author_id != Some(user.id) {
        return Err(CommentError::Denied);
    }
    codeyard_db_repos::repositories::commit_comments::validate_body(input.body).map_err(|_| {
        CommentError::Invalid("A comment needs a nonempty body of at most 65,536 characters")
    })?;
    let authors = authors(pool, cluster, &repository, &[user.id]).await?;
    let changed = codeyard_db_repos::repositories::commit_comments::edit_comment(pool, input)
        .await
        .map_err(write_error)?
        .ok_or(CommentError::Missing)?;
    let entry = details(changed.comment, &authors);
    notify(
        pool,
        cluster,
        broadcaster,
        &user,
        &repository,
        &owner,
        &entry.comment,
        Some(&changed.previous_body),
    )
    .await;
    Ok(entry)
}

pub async fn delete(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    id: i64,
    revision: Option<&str>,
) -> Result<CommitComment> {
    let actor = actor.into();
    writable(pool, actor.user_id, repo).await?;
    // GitHub sends commit_comment only for creation. Deletion needs no Git metadata lookup.
    codeyard_db_repos::repositories::commit_comments::delete_comment(
        pool, actor, repo, id, revision,
    )
    .await
    .map_err(write_error)?
    .ok_or(CommentError::Missing)
}
