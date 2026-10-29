use super::*;
use crate::backend::repositories::{
    git_sessions::validation::ZERO_SHA,
    imports::{GitImport, ImportCredential, MAX_IMPORT_PACK_BYTES},
};

impl LocalStore {
    pub(in crate::backend::repositories::store) async fn import_git(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        import: GitImport,
        pack: &[u8],
    ) -> Result<(), RepoError> {
        if pack.len() > MAX_IMPORT_PACK_BYTES || import.refs.len() > git::MAX_REPOSITORY_REFS {
            return Err(RepoError::response_too_large());
        }
        for (name, sha) in &import.refs {
            if !name.starts_with("refs/")
                || name == "refs/codeyard"
                || name.starts_with("refs/codeyard/")
                || !git2::Reference::is_valid_name(name)
                || !git::is_full_commit_id(sha)
            {
                return Err(super::super::invalid_ref("import"));
            }
        }
        let publication = Arc::new(
            git::locks::publication(self.path())
                .await
                .map_err(RepoError::from)?,
        );
        publication
            .scope(self.import_git_locked(pool, repo_id, actor, import, pack, &publication))
            .await
    }

    async fn import_git_locked(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        import: GitImport,
        pack: &[u8],
        publication: &Arc<git::locks::PublicationGuard>,
    ) -> Result<(), RepoError> {
        self.import_authority(pool, repo_id, actor, &import).await?;
        // Failed preparations may leave intents, not published user refs. They must not
        // make an otherwise empty repository permanently impossible to import into.
        if self
            .mirror_refs()
            .await?
            .keys()
            .any(|name| !name.starts_with("refs/codeyard/"))
        {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Import requires an empty repository",
            ));
        }
        let temp = tempfile::NamedTempFile::new()
            .map_err(|_| unavailable("Import storage unavailable"))?;
        if !import.refs.is_empty() {
            tokio::fs::write(temp.path(), pack)
                .await
                .map_err(|_| unavailable("Import storage unavailable"))?;
            self.index_pack(temp.path(), true).await?;
        }
        for (name, sha) in &import.refs {
            let branch = name.strip_prefix("refs/heads/");
            let write = branch.map(|branch| BranchWrite {
                branch,
                old: None,
                new: Some(sha),
                via: WriteVia::Direct,
            });
            live_write(
                pool,
                repo_id,
                actor,
                write.as_ref(),
                &History::Stored {
                    repository: self.path(),
                },
                self.path(),
                self.node_id,
            )
            .await?;
            // A tag may not name a missing or malformed object either.
            self.git_object_kind(sha).await?;
        }
        // The imported names must fit the same count and byte limits as any other branch or tag.
        let held = self.blocking(git::published_refs_in_use).await?;
        let created: Vec<&str> = import
            .refs
            .keys()
            .filter(|name| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            .map(String::as_str)
            .collect();
        ensure_ref_room(held, &created)?;
        let tags: Vec<_> = import
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
            .map(|(name, sha)| (name.clone(), sha.clone()))
            .collect();
        self.blocking(move |path| git::check_tag_budget(path, None, &tags))
            .await?;
        // A reserved immutable tag keeps its published object, even in a repository recreated at
        // the same owner and name.
        let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .ok_or_else(RepoError::write_denied)?;
        let tag_updates: Vec<_> = import
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
            .map(
                |(name, sha)| crate::backend::repositories::git_sessions::validation::RefUpdate {
                    reference: name.clone(),
                    old: editor::ZERO.to_owned(),
                    new: sha.clone(),
                },
            )
            .collect();
        crate::backend::repositories::releases::immutable::check_current_tag_updates(
            pool,
            &repository,
            &tag_updates,
        )
        .await?;
        self.import_authority(pool, repo_id, actor, &import).await?;
        let writes = import
            .refs
            .iter()
            .filter_map(|(name, sha)| {
                name.strip_prefix("refs/heads/").map(|branch| BranchWrite {
                    branch,
                    old: None,
                    new: Some(sha.as_str()),
                    via: WriteVia::Direct,
                })
            })
            .collect::<Vec<_>>();
        let writes = writes.iter().collect::<Vec<_>>();
        let _change = self
            .begin_branch_change(pool, repo_id, actor, &writes, publication, None)
            .await?;
        // Recheck import cancellation and credential authority after metadata preparation.
        self.import_authority(pool, repo_id, actor, &import).await?;
        // An import publishes its commits like a push, so a token without the workflow
        // permission cannot add workflow files this way either.
        if let ImportCredential::Token(token) = &import.credential {
            let updates = import
                .refs
                .values()
                .map(|sha| (ZERO_SHA.to_owned(), sha.clone()))
                .collect();
            self.check_workflow_grant(pool, Some(*token), updates, None)
                .await?;
        }
        let mut refs = self.mirror_refs().await?;
        refs.extend(import.refs.clone());
        self.set_refs(&refs).await?;
        if let Some(branch) = &import.default_branch {
            if git::branch_ref(branch).is_some_and(|reference| import.refs.contains_key(&reference))
            {
                self.set_head_target(&format!("refs/heads/{branch}"))
                    .await?;
                db::repositories::records::update_repo_default_branch(pool, repo_id, branch)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?;
            }
        }
        refresh_storage_size(pool.clone(), repo_id, self.clone());
        Ok(())
    }

    async fn import_authority(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
        import: &GitImport,
    ) -> Result<(), RepoError> {
        let repository = live_write(
            pool,
            repo_id,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let user = import
            .credential
            .user(pool)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .ok_or_else(RepoError::write_denied)?;
        if user.id != actor
            || !access::can_admin(pool, &user, &repository)
                .await
                .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::write_denied());
        }
        let current = db::source_imports::get(pool, repo_id)
            .await
            .map_err(|_| RepoError::authority_unavailable())?;
        if !current
            .is_some_and(|current| current.generation == import.generation && current.running())
        {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Import is no longer running",
            ));
        }
        Ok(())
    }

    pub(in crate::backend::repositories::store) async fn cancel_import(
        &self,
        pool: &db::DbPool,
        repo_id: i64,
        actor: i64,
    ) -> Result<(), RepoError> {
        let _publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        live_read(pool, repo_id, actor, self.path(), self.node_id).await?;
        db::source_imports::cancel(pool, actor, repo_id)
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<db::source_imports::ImportRefusal>()
                    .is_some()
                {
                    RepoError::write_denied()
                } else {
                    RepoError::authority_unavailable()
                }
            })
    }
}
