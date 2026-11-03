use super::*;
use crate::backend::{
    db::{
        gists::{self, Admission, GistRefusal},
        users::accounts::AccountRestricted,
    },
    repositories::store::{GistCredential, GistEdit, GistRevision, GistSnapshot},
};

fn failure(error: anyhow::Error) -> RepoError {
    match error.downcast_ref::<GistRefusal>() {
        Some(GistRefusal::Missing) => RepoError::new(ErrorKind::NotFound, "Gist not found"),
        Some(GistRefusal::Conflict) => {
            RepoError::new(ErrorKind::Conflict, "Gist changed; retry the request")
        }
        Some(GistRefusal::Denied) => RepoError::write_denied(),
        Some(GistRefusal::Invalid) => RepoError::new(ErrorKind::Invalid, "Invalid gist input"),
        None if error.is::<AccountRestricted>() => RepoError::write_denied(),
        None => RepoError::authority_unavailable(),
    }
}
impl LocalStore {
    async fn gist_location(
        &self,
        pool: &db::DbPool,
        id: i64,
        read: bool,
    ) -> Result<db::models::Repository, RepoError> {
        self.admitted_path(read).await?;
        let repo = db::repositories::records::get_repository_by_id(pool, id)
            .await
            .map_err(failure)?
            .filter(|repo| repo.is_gist)
            .ok_or_else(|| RepoError::new(ErrorKind::NotFound, "Gist not found"))?;
        if !super::repository_matches_path(&repo, self.path(), self.node_id) {
            let current_term = read
                && self.term.get().is_some()
                && self
                    .context
                    .as_ref()
                    .is_some_and(|(_, repository)| *repository == id);
            let replica = if current_term {
                true
            } else if let Some(node) = self.node_id.flatten().filter(|_| read) {
                self.path().file_name() == std::path::Path::new(&repo.disk_path).file_name()
                    && self
                        .path()
                        .parent()
                        .and_then(std::path::Path::file_name)
                        .is_some_and(|name| name == "mirrors")
                    && db::cluster::replica_applied_version(pool, id, node)
                        .await
                        .map_err(failure)?
                        .is_some_and(|version| version >= 0)
            } else {
                false
            };
            if !replica {
                return Err(RepoError::storage_node_unavailable());
            }
        }
        if gists::by_repository(pool, id)
            .await
            .map_err(failure)?
            .is_none()
        {
            return Err(RepoError::new(ErrorKind::NotFound, "Gist not found"));
        }
        Ok(repo)
    }
    /// Finishes a write that was admitted but not recorded as done. Readers take the
    /// publication lease only when one is pending, so reads of a settled gist never wait on it.
    async fn recover_gist(&self, pool: &db::DbPool, id: i64) -> Result<(), RepoError> {
        if gists::pending(pool, id).await.map_err(failure)?.is_none() {
            return Ok(());
        }
        let _publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        self.finish_pending(pool, id).await
    }
    /// The caller holds the publication lease, which also covers a git child that outlived a
    /// restart. It is never held across a SQL transaction.
    async fn finish_pending(&self, pool: &db::DbPool, id: i64) -> Result<(), RepoError> {
        if let Some(pending) = gists::pending(pool, id).await.map_err(failure)? {
            let head = self.refs().await?.remove("refs/heads/main");
            let published = match head {
                Some(head) if head == pending.sha => true,
                Some(head) if Some(&head) == pending.base.as_ref() => false,
                Some(head) => match self.is_ancestor(&pending.sha, &head).await {
                    Ok(published) => published,
                    Err(error)
                        if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) =>
                    {
                        false
                    }
                    Err(error) => return Err(error),
                },
                None => false,
            };
            gists::finish(pool, id, &pending.sha, published, self.node_id)
                .await
                .map_err(failure)?;
            if published {
                git::maintenance::after_push(id, self.path().to_owned());
                refresh_storage_size(pool.clone(), id, self.clone());
            }
        }
        Ok(())
    }
    pub(in crate::backend::repositories::store) async fn gist_snapshot(
        &self,
        pool: &db::DbPool,
        id: i64,
        revision: Option<&str>,
    ) -> Result<GistSnapshot, RepoError> {
        let repo = self.gist_location(pool, id, true).await?;
        if super::repository_matches_path(&repo, self.path(), self.node_id) {
            self.recover_gist(pool, id).await?;
        }
        // Recovery takes the publication lease, which includes this lock, so lock afterwards.
        let _lock = git::locks::lock(self.path()).await;
        let revision = revision.map(str::to_owned);
        self.blocking(move |path| {
            let _lock = _lock;
            git::gists::snapshot(path, revision.as_deref())
        })
        .await
    }
    pub(in crate::backend::repositories::store) async fn gist_history(
        &self,
        pool: &db::DbPool,
        id: i64,
        revision: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<GistRevision>, RepoError> {
        let repo = self.gist_location(pool, id, true).await?;
        if super::repository_matches_path(&repo, self.path(), self.node_id) {
            self.recover_gist(pool, id).await?;
        }
        let lock = git::locks::lock(self.path()).await;
        git::gists::history(self.path(), revision, offset, limit, lock)
            .await
            .map_err(RepoError::from)
    }
    pub(in crate::backend::repositories::store) async fn gist_write(
        &self,
        pool: &db::DbPool,
        id: i64,
        actor: i64,
        credential: GistCredential,
        input: GistEdit,
    ) -> Result<GistSnapshot, RepoError> {
        input.validate()?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let repo = self.gist_location(pool, id, false).await?;
        if repo.owner_user_id != Some(actor) {
            return Err(RepoError::write_denied());
        }
        super::require_legacy_publisher(pool, id).await?;
        self.finish_pending(pool, id).await?;
        let user = db::users::accounts::get_user_by_id(pool, actor)
            .await
            .map_err(failure)?
            .filter(|user| !user.is_banned && !user.must_change_password)
            .ok_or_else(RepoError::write_denied)?;
        let edit = input.clone();
        let email = user.noreply_email();
        let prepared = self
            .blocking(move |path| git::gists::prepare(path, &edit, &user.username, &email))
            .await?;
        let _change = super::begin_ref_change(pool, id, self.path(), self.node_id).await?;
        gists::admit(
            pool,
            actor,
            &credential.borrowed(),
            &Admission {
                repo_id: id,
                disk_path: &repo.disk_path,
                storage_node_id: self.node_id,
                base: prepared.base.as_deref(),
                sha: &prepared.sha,
                description: input.description.as_deref(),
            },
        )
        .await
        .map_err(failure)?;
        // Admission is durable and single-use. A lost response is reconciled from the actual ref.
        if publication
            .scope(git::publish_ref(
                self.path(),
                "update gist",
                "refs/heads/main",
                Some(&prepared.sha),
                prepared
                    .base
                    .as_deref()
                    .unwrap_or("0000000000000000000000000000000000000000"),
            ))
            .await
            .is_err()
        {
            let published = self.refs().await?.get("refs/heads/main") == Some(&prepared.sha);
            if !published {
                gists::finish(pool, id, &prepared.sha, false, self.node_id)
                    .await
                    .map_err(failure)?;
                return Err(RepoError::new(
                    ErrorKind::Conflict,
                    "Gist changed during publication",
                ));
            }
        }
        if gists::finish(pool, id, &prepared.sha, true, self.node_id)
            .await
            .is_err()
        {
            return Err(RepoError::applied_followup_failed());
        }
        // Each edit writes loose objects, so it schedules packing as a push does.
        git::maintenance::after_push(id, self.path().to_owned());
        refresh_storage_size(pool.clone(), id, self.clone());
        self.blocking(move |path| git::gists::snapshot(path, Some(&prepared.sha)))
            .await
    }
}
