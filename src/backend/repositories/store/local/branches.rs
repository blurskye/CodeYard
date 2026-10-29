use super::*;

impl LocalStore {
    pub(in crate::backend::repositories::store) async fn rename_branch(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        old: &str,
        new: &str,
    ) -> Result<String, RepoError> {
        let (store, pool, old, new) = (self.clone(), pool.clone(), old.to_owned(), new.to_owned());
        // Finish metadata and HEAD updates even if the HTTP client leaves after the ref moves.
        tokio::spawn(async move {
            store
                .rename_branch_owned(&pool, repo, actor, &old, &new)
                .await
        })
        .await
        .map_err(|_| RepoError::applied_followup_failed())?
    }

    async fn rename_branch_owned(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        old: &str,
        new: &str,
    ) -> Result<String, RepoError> {
        let old_ref = git::branch_ref(old).ok_or_else(|| super::super::invalid_ref("branch"))?;
        let new_ref = git::branch_ref(new).ok_or_else(|| super::super::invalid_ref("branch"))?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        let user = self.rename_authority(pool, repo, actor, old, new).await?;
        let refs = self.refs().await?;
        let target = refs
            .get(&old_ref)
            .ok_or_else(|| RepoError::new(ErrorKind::NotFound, "Branch not found"))?
            .clone();
        let rules = db::repositories::protection::list_branch_protection_rules(pool, repo)
            .await
            .map_err(|_| RepoError::authority_unavailable())?;
        if refs.contains_key(&new_ref) || rules.iter().any(|rule| rule.pattern == new) {
            return Err(RepoError::new(ErrorKind::Invalid, "Branch already exists"));
        }
        // A protection pattern holds at most 255 characters. Refuse a longer name before any ref
        // moves, instead of moving the branch and failing to carry the rule over.
        if new.chars().count() > 255 && rules.iter().any(|rule| rule.pattern == old) {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "Protected branch names are limited to 255 characters",
            ));
        }
        // The new name counts against the ref-name budget in place of the old one.
        let mut held = self.blocking(git::branches_and_tags_in_use).await?;
        held.count = held.count.saturating_sub(1);
        held.bytes = held.bytes.saturating_sub(git::ref_listing_bytes(&old_ref));
        super::ensure_ref_room(held, &[&new_ref])?;
        let advertised_default = git::repository::default_branch(self.path())
            .await
            .map_err(RepoError::from)?;
        let _change = begin_ref_change(pool, repo, self.path(), self.node_id).await?;
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
        self.blocking(move |path| {
            let _held = held;
            git::prepare_branch_publications(
                path,
                repo,
                git::PublicationActor::User(actor),
                &username,
                &updates,
            )
        })
        .await?;
        self.rename_authority(pool, repo, actor, old, new).await?;
        publication
            .scope(git::publish_branch_rename(self.path(), old, new, &target))
            .await
            .map_err(RepoError::from)?;
        let followup = async {
            if advertised_default.as_deref() == Some(old) {
                self.set_head_target(&new_ref).await?;
            }
            db::repositories::protection::rename_branch_settings(pool, actor, repo, old, new)
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
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        old: &str,
        new: &str,
    ) -> Result<db::models::User, RepoError> {
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
            protected |= !crate::backend::repositories::protection::matching_rules(
                pool,
                &repository,
                branch,
            )
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .is_empty();
        }
        if protected
            && !access::can_admin(pool, &user, &repository)
                .await
                .map_err(|_| RepoError::authority_unavailable())?
        {
            return Err(RepoError::write_denied());
        }
        Ok(user)
    }

    pub(in crate::backend::repositories::store) async fn merge_upstream(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        source: (i64, &LocalStore),
        branch: &str,
    ) -> Result<super::super::UpstreamMerge, RepoError> {
        use super::super::{UpstreamMerge, UpstreamMergeType};
        let reference =
            git::branch_ref(branch).ok_or_else(|| super::super::invalid_ref("branch"))?;
        self.upstream_authority(pool, repo, actor, source).await?;
        let target = git::repository::current_branch_head(source.1.path(), branch)
            .await
            .map_err(RepoError::from)?
            .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Upstream branch not found"))?;
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        self.upstream_authority(pool, repo, actor, source).await?;
        let old = git::repository::current_branch_head(self.path(), branch)
            .await
            .map_err(RepoError::from)?
            .ok_or_else(|| RepoError::new(ErrorKind::Invalid, "Branch not found"))?;
        let user = db::users::accounts::get_user_by_id(pool, actor)
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
        self.upstream_authority(pool, repo, actor, source).await?;
        publication
            .scope(git::repository::publish_pull_branch_update(
                self.path(),
                branch,
                &old,
                &prepared,
            ))
            .await
            .map_err(RepoError::from)?;
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
        followup?;
        Ok(UpstreamMerge {
            old,
            sha: target,
            merge_type,
        })
    }

    async fn upstream_authority(
        &self,
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        source: (i64, &LocalStore),
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
        live_read(pool, source.0, actor, source.1.path(), source.1.node_id).await
    }
}
