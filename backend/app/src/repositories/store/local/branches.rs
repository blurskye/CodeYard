use crate::repositories::protection::BranchWrite;
use crate::repositories::protection::History;
use crate::repositories::protection::WriteVia;
use crate::repositories::store::local::ensure_ref_room;
use crate::repositories::store::local::live_read;
use crate::repositories::store::local::live_write;
use crate::repositories::store::local::record_branch_update;
use crate::repositories::store::local::record_secret_bypasses;
use crate::repositories::store::local::LocalStore;
use crate::repositories::store::local::RefChange;

impl LocalStore {
    /// Boxed with an explicit `Send` bound. This rename and `publish_file` each spawn a future
    /// whose call graph reaches the other, so proving either one `Send` would need the other's
    /// type, a cycle the compiler rejects.
    pub(crate) fn rename_branch(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        authority: codeyard_db_core::queries::credentials::Actor,
        old: &str,
        new: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, RepoError>> + Send>>
    {
        let (store, pool, old, new) = (self.clone(), pool.clone(), old.to_owned(), new.to_owned());
        Box::pin(async move {
            // Finish metadata and HEAD updates even if the HTTP client leaves after the ref moves.
            tokio::spawn(async move {
                store
                    .rename_branch_owned(&pool, repo, authority, &old, &new)
                    .await
            })
            .await
            .map_err(|_| RepoError::applied_followup_failed())?
        })
    }

    async fn rename_branch_owned(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        authority: codeyard_db_core::queries::credentials::Actor,
        old: &str,
        new: &str,
    ) -> Result<String, RepoError> {
        self.admitted_path(false).await?;
        let actor = authority.user_id;
        let old_ref =
            git::branch_ref(old).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        let new_ref =
            git::branch_ref(new).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let user = self
            .rename_authority(pool, repo, &authority, old, new)
            .await?;
        let refs = self.refs().await?;
        let target = refs
            .get(&old_ref)
            .ok_or_else(|| RepoError::new(ErrorKind::NotFound, "Branch not found"))?
            .clone();
        let rules =
            codeyard_db_repos::repositories::protection::list_branch_protection_rules(pool, repo)
                .await
                .map_err(|_| RepoError::authority_unavailable())?;
        if refs.contains_key(&new_ref) || rules.iter().any(|rule| rule.pattern == new) {
            return Err(RepoError::new(ErrorKind::Invalid, "Branch already exists"));
        }
        // Pull-request branch columns and protection patterns both hold at most 255 characters.
        // Validate before publication even when this branch has no exact protection rule.
        if new.chars().count() > 255 {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Branch names are limited to 255 characters",
            ));
        }
        // The new name counts against the ref-name budget in place of the old one.
        let mut held = self.blocking(git::published_refs_in_use).await?;
        held.count = held.count.saturating_sub(1);
        held.bytes = held.bytes.saturating_sub(git::ref_listing_bytes(&old_ref));
        crate::repositories::store::local::ensure_ref_room(held, &[&new_ref])?;
        let advertised_default = git::repository::default_branch(self.path())
            .await
            .map_err(RepoError::from)?;
        let _change = self.begin_publisher_change(pool, repo).await?;
        let quorum = self.quorum_context(pool, repo).await?.is_some();
        let updates = vec![
            git::PublicationUpdate {
                reference: old_ref.clone(),
                old: Some(target.clone()),
                new: None,
            },
            git::PublicationUpdate {
                reference: new_ref.clone(),
                old: None,
                new: Some(target.clone()),
            },
        ];
        let held = publication.clone();
        let username = user.username;
        let planned = self
            .blocking(move |path| {
                let _held = held;
                if quorum {
                    git::plan_branch_publications(
                        path,
                        repo,
                        git::PublicationActor::User(actor),
                        &username,
                        &updates,
                    )
                } else {
                    git::prepare_branch_publications(
                        path,
                        repo,
                        git::PublicationActor::User(actor),
                        &username,
                        &updates,
                    )?;
                    Ok(Vec::new())
                }
            })
            .await?;
        let mut changes = vec![
            oid_change(&old_ref, Some(&target), None),
            oid_change(&new_ref, None, Some(&target)),
        ];
        changes.extend(planned.iter().map(|change| {
            oid_change(
                &change.reference,
                change.old.as_deref(),
                change.new.as_deref(),
            )
        }));
        if advertised_default.as_deref() == Some(old) {
            changes.push(codeyard_db_core::models::cluster::RefChange {
                name: "HEAD".into(),
                kind: "symbolic".into(),
                old_target: Some(old_ref.clone()),
                new_target: Some(new_ref.clone()),
            });
        }
        self.rename_authority(pool, repo, &authority, old, new)
            .await?;
        if !publication
            .scope(self.publish_quorum_refs(pool, repo, &changes, async {
                self.rename_authority(pool, repo, &authority, old, new)
                    .await?;
                Ok(())
            }))
            .await?
        {
            publication
                .scope(git::publish_branch_rename(self.path(), old, new, &target))
                .await
                .map_err(RepoError::from)?;
            if advertised_default.as_deref() == Some(old) {
                self.set_head_target(&new_ref).await?;
            }
        }
        let followup = async {
            codeyard_db_repos::repositories::protection::rename_branch_settings(
                pool, actor, repo, old, new,
            )
            .await
            .map_err(|_| RepoError::applied_followup_failed())?;
            self.after_ref_change(
                pool,
                repo,
                actor,
                &[
                    (old_ref, target.clone(), editor::ZERO.into()),
                    (new_ref, editor::ZERO.into(), target.clone()),
                ],
            )
            .await
        }
        .await;
        followup.map_err(|_| RepoError::applied_followup_failed())?;
        Ok(target)
    }

    async fn rename_authority(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        authority: &codeyard_db_core::queries::credentials::Actor,
        old: &str,
        new: &str,
    ) -> Result<codeyard_db_core::models::User, RepoError> {
        let actor = authority.user_id;
        let (repository, user) = access::check_write(pool, repo, actor)
            .await
            .map_err(|error| match error {
                access::AccessError::Unavailable => RepoError::authority_unavailable(),
                _ => RepoError::write_denied(),
            })?;
        live_write(
            pool,
            repo,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        let mut protected = repository.default_branch == old;
        for branch in [old, new] {
            protected |=
                !crate::repositories::protection::matching_rules(pool, &repository, branch)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .is_empty();
        }
        // Renaming a protected or default branch needs a token's administration permission.
        if let Some(token) = authority.token_id() {
            if !codeyard_identity::tokens::repository_scope(
                pool,
                token,
                actor,
                repo,
                if protected {
                    "administration"
                } else {
                    "contents"
                },
                true,
            )
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            {
                return Err(RepoError::write_denied());
            }
        }
        if protected
            && !access::can_admin(pool, &user, &repository)
                .await
                .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::write_denied());
        }
        access::check_publication_actor(pool, authority, &repository)
            .await
            .map_err(RepoError::from)?;
        Ok(user)
    }

    pub(crate) async fn merge_upstream(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
        source: (i64, &LocalStore),
        branch: &str,
        token: Option<i64>,
    ) -> Result<crate::repositories::store::types::UpstreamMerge, RepoError> {
        use crate::repositories::store::types::UpstreamMerge;
        use crate::repositories::store::types::UpstreamMergeType;
        self.admitted_path(false).await?;
        source.1.admitted_path(true).await?;
        let reference =
            git::branch_ref(branch).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        self.upstream_authority(pool, repo, actor, source, token)
            .await?;
        let target = git::repository::current_branch_head(source.1.path(), branch)
            .await
            .map_err(RepoError::from)?
            .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Upstream branch not found"))?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        self.upstream_authority(pool, repo, actor, source, token)
            .await?;
        let old = git::repository::current_branch_head(self.path(), branch)
            .await
            .map_err(RepoError::from)?
            .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Branch not found"))?;
        let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .ok_or_else(RepoError::write_denied)?;
        let email = user.commit_email();
        let prepared = git::repository::prepare_upstream_merge(
            self.path(),
            source.1.path(),
            &old,
            &target,
            &git::Person {
                name: &user.username,
                email: &email,
                date: None,
            },
        )
        .await
        .map_err(RepoError::from)?;
        let sha = prepared.head().to_owned();
        let merge_type = if sha == old {
            return Ok(UpstreamMerge {
                old,
                sha,
                merge_type: UpstreamMergeType::None,
            });
        } else if sha == target {
            UpstreamMergeType::FastForward
        } else {
            UpstreamMergeType::Merge
        };
        let target = sha;
        let write = BranchWrite {
            branch,
            old: Some(&old),
            new: Some(&target),
            via: WriteVia::ForkSync,
        };
        let _change = self
            .begin_branch_change(
                pool,
                repo,
                actor,
                &[&write],
                &publication,
                Some(prepared.path()),
            )
            .await?;
        self.upstream_authority(pool, repo, actor, source, token)
            .await?;
        self.check_publication_grant(
            pool,
            repo,
            actor,
            token,
            vec![(old.clone(), target.clone())],
            Some(prepared.path().to_owned()),
        )
        .await?;
        let repository = access::current_repository(pool, repo)
            .await
            .map_err(|_| RepoError::write_denied())?;
        let quarantine = prepared.path().join("objects");
        let bypasses =
            crate::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                pool,
                &repository,
                actor,
                self.path(),
                Some(&quarantine),
                std::slice::from_ref(&target),
            )
            .await
            .map_err(RepoError::from)?;
        self.import_prepared_objects(pool, repo, prepared.path(), std::slice::from_ref(&target))
            .await?;
        let changes = [oid_change(&reference, Some(&old), Some(&target))];
        if !publication
            .scope(self.publish_quorum_refs(pool, repo, &changes, async {
                self.upstream_authority(pool, repo, actor, source, token)
                    .await?;
                live_write(
                    pool,
                    repo,
                    actor,
                    Some(&write),
                    &History::Stored {
                        repository: prepared.path(),
                    },
                    self.path(),
                    self.node_id,
                )
                .await?;
                self.check_publication_grant(
                    pool,
                    repo,
                    actor,
                    token,
                    vec![(old.clone(), target.clone())],
                    Some(prepared.path().to_owned()),
                )
                .await
            }))
            .await?
        {
            publication
                .scope(git::repository::publish_pull_branch_update(
                    self.path(),
                    branch,
                    &old,
                    &prepared,
                ))
                .await
                .map_err(RepoError::from)?;
        }
        let followup = self
            .after_ref_change(
                pool,
                repo,
                actor,
                &[(reference, old.clone(), target.clone())],
            )
            .await;
        record_branch_update(
            pool,
            repo,
            actor,
            (self.path(), self.node_id),
            branch,
            Some(&old),
            &target,
        )
        .await;
        let bypass_followup = record_secret_bypasses(pool, repo, actor, bypasses).await;
        followup?;
        bypass_followup?;
        Ok(UpstreamMerge {
            old,
            sha: target,
            merge_type,
        })
    }

    async fn upstream_authority(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
        source: (i64, &LocalStore),
        token: Option<i64>,
    ) -> Result<(), RepoError> {
        let (repository, _) = access::check_write(pool, repo, actor)
            .await
            .map_err(|_| RepoError::write_denied())?;
        if !repository.is_fork || repository.forked_from_id != Some(source.0) {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Repository is not a fork",
            ));
        }
        live_write(
            pool,
            repo,
            actor,
            None,
            &History::SingleCommit,
            self.path(),
            self.node_id,
        )
        .await?;
        live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await?;
        if let Some(token) = token {
            let credential = crate::repositories::git_sessions::ReceiveCredential::Token(token);
            let upstream = access::current_repository(pool, source.0)
                .await
                .map_err(|_| RepoError::write_denied())?;
            for (repository, write) in [(&repository, true), (&upstream, false)] {
                let (user, _) = credential
                    .authorize(pool, repository, write)
                    .await
                    .map_err(|_| RepoError::write_denied())?;
                if user.id != actor {
                    return Err(RepoError::write_denied());
                }
            }
        }
        Ok(())
    }
}
