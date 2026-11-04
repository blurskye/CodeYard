use super::*;
use crate::backend::repositories::{
    git_sessions::validation::ZERO_SHA,
    imports::{GitImport, ImportCredential, MAX_IMPORT_PACK_BYTES},
};

impl LocalStore {
    pub(in crate::backend::repositories::store) async fn import_git(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo_id: i64,
        actor: i64,
        mut import: GitImport,
        pack: &[u8],
    ) -> Result<(), RepoError> {
        self.admitted_path(false).await?;
        // Pull refs are local discussion identities, never source-import metadata.
        import
            .refs
            .retain(|name, _| !name.starts_with("refs/pull/"));
        if pack.len() > MAX_IMPORT_PACK_BYTES || import.refs.len() > git::MAX_REPOSITORY_REFS {
            return Err(RepoError::response_too_large());
        }
        for (name, sha) in &import.refs {
            if !name.starts_with("refs/")
                || ((name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
                    && !git::is_canonical_ref(name))
                || name == "refs/codeyard"
                || name.starts_with("refs/codeyard/")
                || !git2::Reference::is_valid_name(name)
                || !git::is_full_commit_id(sha)
            {
                return Err(codeyard_git::error::invalid_ref("import"));
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
        pool: &codeyard_db_core::DbPool,
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
        // All object inspection happens in isolated storage. Refusal must not retain the
        // incoming pack in a repository whose maintenance deliberately never prunes objects.
        let quarantine =
            tempfile::tempdir().map_err(|_| unavailable("Import storage unavailable"))?;
        git::repository::initialize_repository(quarantine.path(), None, "main")
            .await
            .map_err(RepoError::from)?;
        let staged = LocalStore::new(
            quarantine
                .path()
                .to_str()
                .ok_or_else(|| unavailable("Import storage unavailable"))?,
            None,
        );
        let temp = tempfile::NamedTempFile::new()
            .map_err(|_| unavailable("Import storage unavailable"))?;
        if !import.refs.is_empty() {
            tokio::fs::write(temp.path(), pack)
                .await
                .map_err(|_| unavailable("Import storage unavailable"))?;
            staged.index_pack(temp.path(), true).await?;
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
                    repository: staged.path(),
                },
                self.path(),
                self.node_id,
            )
            .await?;
            // A tag may not name a missing or malformed object either.
            staged.git_object_kind(sha).await?;
        }
        // Imported public refs share the count and byte limits of ordinary publication.
        let held = self.blocking(git::published_refs_in_use).await?;
        let created: Vec<&str> = import
            .refs
            .keys()
            .filter(|name| git::is_public_ref(name))
            .map(String::as_str)
            .collect();
        ensure_ref_room(held, &created)?;
        let tags: Vec<_> = import
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
            .map(|(name, sha)| (name.clone(), sha.clone()))
            .collect();
        staged
            .blocking(move |path| git::check_tag_budget(path, None, &tags))
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
            .begin_branch_change(
                pool,
                repo_id,
                actor,
                &writes,
                publication,
                Some(staged.path()),
            )
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
            // The imported objects are only in the staging repository until publication.
            staged
                .check_publication_grant(pool, repo_id, actor, Some(*token), updates, None)
                .await?;
        }
        let mut refs = self.mirror_refs().await?;
        refs.extend(import.refs.clone());
        if refs.len() > git::MAX_MIRROR_REFS
            || refs.keys().any(|name| name.len() > 4096)
            || refs
                .keys()
                .try_fold(0usize, |bytes, name| {
                    bytes.checked_add(git::repository::ref_listing_bytes(name))
                })
                .is_none_or(|bytes| bytes > git::MAX_MIRROR_REF_BYTES)
        {
            return Err(RepoError::response_too_large());
        }
        let branch = import
            .default_branch
            .as_deref()
            .filter(|branch| import.refs.contains_key(&format!("refs/heads/{branch}")))
            .or_else(|| {
                crate::backend::repositories::reads::preferred_branch(
                    import
                        .refs
                        .keys()
                        .filter_map(|name| name.strip_prefix("refs/heads/")),
                )
            });
        // Finish fallible default-branch preparation before publishing user refs. A failed
        // import remains empty and retryable, even if HEAD already names the future branch.
        if let Some(branch) = branch {
            let target = format!("refs/heads/{branch}");
            let old = self.head_target().await?;
            let changes = [codeyard_db_core::models::RefChange {
                name: "HEAD".into(),
                kind: "symbolic".into(),
                old_target: old,
                new_target: Some(target.clone()),
            }];
            if !publication
                .scope(self.publish_quorum_refs(
                    pool,
                    repo_id,
                    &changes,
                    self.import_authority(pool, repo_id, actor, &import),
                ))
                .await?
            {
                self.set_head_target(&target).await?;
            }
            db::repositories::records::update_repo_default_branch(pool, repo_id, branch)
                .await
                .map_err(|_| RepoError::authority_unavailable())?;
        }
        self.import_prepared_objects(
            pool,
            repo_id,
            staged.path(),
            &import.refs.values().cloned().collect::<Vec<_>>(),
        )
        .await?;
        let changes = import
            .refs
            .iter()
            .map(|(name, sha)| oid_change(name, None, Some(sha)))
            .collect::<Vec<_>>();
        let quorum = publication
            .scope(self.publish_quorum_refs(pool, repo_id, &changes, async {
                self.import_authority(pool, repo_id, actor, &import).await?;
                for write in &writes {
                    live_write(
                        pool,
                        repo_id,
                        actor,
                        Some(*write),
                        &History::Stored {
                            repository: staged.path(),
                        },
                        self.path(),
                        self.node_id,
                    )
                    .await?;
                }
                crate::backend::repositories::releases::immutable::check_current_tag_updates(
                    pool,
                    &repository,
                    &tag_updates,
                )
                .await?;
                if let ImportCredential::Token(token) = &import.credential {
                    staged
                        .check_publication_grant(
                            pool,
                            repo_id,
                            actor,
                            Some(*token),
                            import
                                .refs
                                .values()
                                .map(|sha| (ZERO_SHA.to_owned(), sha.clone()))
                                .collect(),
                            None,
                        )
                        .await?;
                }
                Ok(())
            }))
            .await?;
        // Put refspecs in the isolated repository's config rather than argv: a valid mirror
        // can exceed the host argument-size limit. No tracking refs exist, so the lease
        // requires every imported destination ref to remain absent.
        if !quorum && !import.refs.is_empty() {
            use tokio::io::AsyncWriteExt;
            let quote = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
            let destination = std::path::absolute(self.path())
                .map_err(|_| unavailable("Invalid import destination"))?;
            let destination = destination
                .to_str()
                .ok_or_else(|| unavailable("Invalid import destination"))?;
            let mut config = format!(
                "\n[remote \"codeyard-import\"]\n\turl = \"{}\"\n",
                quote(destination)
            );
            for (name, sha) in &import.refs {
                config.push_str(&format!(
                    "\tpush = \"{}\"\n",
                    quote(&format!("{sha}:{name}"))
                ));
            }
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(staged.path().join("config"))
                .await
                .map_err(|_| unavailable("Import storage unavailable"))?;
            file.write_all(config.as_bytes())
                .await
                .map_err(|_| unavailable("Import storage unavailable"))?;
            self.import_authority(pool, repo_id, actor, &import).await?;
            // receive-pack owns quarantine migration and the atomic destination CAS.
            let (status, _) = staged
                .git(&[
                    "push",
                    "--atomic",
                    "--porcelain",
                    "--no-verify",
                    "--force-with-lease",
                    "codeyard-import",
                ])
                .await?;
            if !status.success() {
                let current = self.mirror_refs().await?;
                if !import
                    .refs
                    .iter()
                    .all(|(name, sha)| current.get(name) == Some(sha))
                {
                    return Err(unavailable("Import publication failed"));
                }
            }
        }
        let contributors = if import.refs.is_empty() {
            Ok(())
        } else {
            db::repositories::records::advance_contribution_version(pool, repo_id)
                .await
                .map_err(|_| RepoError::applied_followup_failed())
        };
        for tip in import
            .refs
            .values()
            .collect::<std::collections::HashSet<_>>()
        {
            if let Ok(commit) = self.commit(tip).await {
                if let Err(error) =
                    db::secret_scanning::enqueue(pool, repo_id, &commit.sha, actor).await
                {
                    tracing::warn!(repo_id, %error, "imported history scan could not be queued");
                }
            }
        }
        refresh_storage_size(pool.clone(), repo_id, self.clone());
        contributors
    }

    async fn import_authority(
        &self,
        pool: &codeyard_db_core::DbPool,
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
        let user = match &import.credential {
            ImportCredential::Token(token) => {
                crate::backend::repositories::git_sessions::ReceiveCredential::Token(*token)
                    .authorize(pool, &repository, true)
                    .await
                    .map_err(|_| RepoError::write_denied())?
                    .0
            }
            ImportCredential::Session(session) => {
                db::users::sessions::get_session_user(pool, session)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .ok_or_else(RepoError::write_denied)?
            }
        };
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
        pool: &codeyard_db_core::DbPool,
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
