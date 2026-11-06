//! The home cell validates credentials; resource cells receive only public metadata and bounds.
use anyhow::Result;
use codeyard_db_core::{
    cells::authorization::{Answer, Authority, Home, Proof},
    DbPool,
};
use std::{future::Future, pin::Pin};

pub struct Local {
    pub pool: DbPool,
}
impl Home for Local {
    fn repository_source<'a>(
        &'a self,
        owner: &'a str,
        name: &'a str,
        proof: &'a Proof,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<codeyard_db_core::cells::repositories::Source>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.source(owner, name, proof).await })
    }
    fn cell(&self) -> Option<codeyard_support::cells::Prefix> {
        Some(self.pool.cell_id())
    }
    fn receive_pointer<'a>(
        &'a self,
        event: &'a codeyard_db_core::cells::pointers::Event,
        admission: &'a dyn codeyard_db_core::cells::pointers::Admission,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            codeyard_db_core::cells::pointers::apply(&self.pool, event, admission).await
        })
    }
    fn user<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<codeyard_db_core::models::User>>> + Send + 'a>>
    {
        Box::pin(async move {
            let user =
                codeyard_db_identity::users::accounts::get_user_by_username(&self.pool, name)
                    .await?;
            Ok(self.public_user(user))
        })
    }
    fn user_id<'a>(
        &'a self,
        id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<codeyard_db_core::models::User>>> + Send + 'a>>
    {
        Box::pin(async move {
            codeyard_support::cells::Prefix::for_id(id)?;
            Ok(self.public_user(
                codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, id).await?,
            ))
        })
    }
    fn authorize<'a>(
        &'a self,
        proof: &'a Proof,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Answer>>> + Send + 'a>> {
        Box::pin(async move { self.answer(proof).await })
    }
}
impl Local {
    pub async fn fork_actor(
        &self,
        proof: &Proof,
    ) -> Result<Option<codeyard_db_core::queries::credentials::Actor>> {
        use codeyard_db_core::queries::users::accounts::AccountCredential;
        proof.validate()?;
        let local = match proof {
            Proof::Token { home, .. } | Proof::Session { home, .. } => self.pool.owns_prefix(*home),
            Proof::Ssh { .. } => anyhow::bail!("Fork metadata requires an account credential"),
        };
        let answer = if local {
            self.answer(proof).await?
        } else {
            codeyard_db_core::cells::authorization::authorize(&self.pool, proof).await?
        };
        let Some(answer) = answer else {
            return Ok(None);
        };
        let Some(user) =
            codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, answer.user.id)
                .await?
        else {
            return Ok(None);
        };
        let credential = match proof {
            Proof::Token { hash, .. } => {
                let Some(id) = answer.credential_id else {
                    return Ok(None);
                };
                if user.is_stub {
                    AccountCredential::ForeignToken(id, hash)
                } else {
                    AccountCredential::Token(id, hash)
                }
            }
            Proof::Session { value, .. } => AccountCredential::Session(value),
            Proof::Ssh { .. } => unreachable!(),
        };
        Ok(Some(codeyard_db_core::queries::credentials::Actor::from((
            user.id,
            &credential,
        ))))
    }
    async fn source(
        &self,
        owner: &str,
        name: &str,
        proof: &Proof,
    ) -> Result<Option<codeyard_db_core::cells::repositories::Source>> {
        use codeyard_db_core::cells::repositories::{Owner, Source};
        codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Name, owner)?;
        anyhow::ensure!(
            codeyard_support::repository_names::valid(name),
            "Invalid fork source name"
        );
        let Some(actor) = self.fork_actor(proof).await? else {
            return Ok(None);
        };
        let Some(mut repository) =
            codeyard_db_repos::repositories::records::get_repository(&self.pool, owner, name)
                .await?
                .filter(|repo| {
                    !repo.is_stub
                        && !repo.is_gist
                        && repo.home_cell == i32::from(self.pool.cell_id().get())
                })
        else {
            return Ok(None);
        };
        let Some(authorized) = codeyard_db_repos::repositories::creation::read_fork_source(
            &self.pool,
            &actor,
            repository.id,
        )
        .await?
        else {
            return Ok(None);
        };
        repository = authorized;
        let allow_forking = codeyard_db_repos::repositories::records::repository_features(
            &self.pool,
            repository.id,
        )
        .await?
        .allow_forking;
        let owner = match repository.owner_kind.as_str() {
            "user" => {
                let Some(mut owner) = codeyard_db_identity::users::accounts::get_user_by_id(
                    &self.pool,
                    repository.owner_id(),
                )
                .await?
                else {
                    return Ok(None);
                };
                codeyard_db_core::cells::authorization::redact_user(&mut owner);
                Owner::User(Box::new(owner))
            }
            "org" => {
                let Some(mut owner) =
                    codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
                        &self.pool,
                        repository.owner_id(),
                    )
                    .await?
                else {
                    return Ok(None);
                };
                owner.billing_email = None;
                owner.avatar_file_id = None;
                Owner::Organization(Box::new(owner))
            }
            _ => return Ok(None),
        };
        let Some(current) = codeyard_db_repos::repositories::creation::read_fork_source(
            &self.pool,
            &actor,
            repository.id,
        )
        .await?
        else {
            return Ok(None);
        };
        repository = current;
        repository.forks_count = codeyard_db_repos::repositories::discovery::public_fork_counts(
            &self.pool,
            &[repository.id],
        )
        .await?
        .get(&repository.id)
        .copied()
        .unwrap_or_default();
        repository.disk_path.clear();
        repository.storage_node_id = None;
        let source = Source {
            reader_id: actor.user_id,
            credential_id: actor.token_id(),
            default_branch_oid: None,
            repository,
            owner,
            allow_forking,
            git_url: None,
        };
        source.validate(self.pool.cell_id())?;
        Ok(Some(source))
    }
    fn public_user(
        &self,
        user: Option<codeyard_db_core::models::User>,
    ) -> Option<codeyard_db_core::models::User> {
        user.filter(|user| {
            !user.is_stub
                && !user.is_banned
                && !user.must_change_password
                && user.home_cell == i32::from(self.pool.cell_id().get())
        })
        .map(|mut user| {
            codeyard_db_core::cells::authorization::redact_user(&mut user);
            user
        })
    }
    async fn answer(&self, proof: &Proof) -> Result<Option<Answer>> {
        proof.validate()?;
        let pool = &self.pool;
        let (user, id, write, expires_at, authority) = match proof {
            Proof::Token { home, hash } => {
                if !pool.owns_prefix(*home) {
                    return Ok(None);
                }
                let Some(token) =
                    codeyard_db_identity::users::tokens::find_native_token(pool, hash).await?
                else {
                    return Ok(None);
                };
                let Some(user) =
                    codeyard_db_identity::users::tokens::token_user(pool, token.id, false).await?
                else {
                    return Ok(None);
                };
                let credential =
                    codeyard_db_core::queries::users::accounts::AccountCredential::Token(
                        token.id, hash,
                    );
                let authority = if user.username == codeyard_db_core::queries::ci_tokens::BOT {
                    let Some(job) = codeyard_db_actions::ci_tokens::get(pool, token.id).await?
                    else {
                        return Ok(None);
                    };
                    Authority::Job {
                        repository: job.repo_id,
                        permissions: serde_json::from_str(&job.permissions)?,
                    }
                } else if let Some(bounds) = crate::github_apps::authority(pool, token.id).await? {
                    match bounds.source {
                        crate::github_apps::Source::Personal(grant) => {
                            if !grant.active {
                                return Ok(None);
                            }
                            let (owner_user, owner_org) = match grant.owner {
                                crate::fine_grained::Owner::User(id) => (Some(id), None),
                                crate::fine_grained::Owner::Organization(id) => (None, Some(id)),
                            };
                            let repositories = match grant.access {
                                crate::fine_grained::Access::All => None,
                                crate::fine_grained::Access::Public => Some(vec![]),
                                crate::fine_grained::Access::Selected => Some(
                                    codeyard_db_identity::fine_grained_tokens::repositories(
                                        pool,
                                        token.id,
                                        user.id,
                                        &credential,
                                    )
                                    .await?
                                    .into_iter()
                                    .map(|(id, _)| id)
                                    .collect(),
                                ),
                            };
                            Authority::FineGrained {
                                owner_user,
                                owner_org,
                                repositories,
                                permissions: serde_json::Value::Object(grant.permissions),
                            }
                        }
                        crate::github_apps::Source::App {
                            installation: Some(id),
                            repositories,
                            ..
                        } => {
                            let Some(installation) =
                                codeyard_db_identity::github_apps::installation(pool, id)
                                    .await?
                                    .filter(|installation| installation.suspended_at.is_none())
                            else {
                                return Ok(None);
                            };
                            // A selected installation cannot acquire all-repository scope in transit.
                            let selected = if installation.all_repos && repositories.is_empty() {
                                None
                            } else {
                                let (total, installed) =
                                    codeyard_db_identity::github_apps::installation_repositories(
                                        pool,
                                        id,
                                        None,
                                        &repositories,
                                        0,
                                        1001,
                                    )
                                    .await?;
                                if total > 1000 {
                                    return Ok(None);
                                }
                                Some(installed.into_iter().map(|repo| repo.id).collect())
                            };
                            Authority::App {
                                owner_user: installation.user_id,
                                owner_org: installation.org_id,
                                repositories: selected,
                                permissions: bounds.permissions,
                            }
                        }
                        crate::github_apps::Source::App {
                            installation: None, ..
                        } => return Ok(None),
                    }
                } else if let Some(oauth) =
                    codeyard_db_identity::oauth_apps::token(pool, token.id).await?
                {
                    Authority::OAuth {
                        client_id: oauth.client_id,
                        scopes: oauth.scopes,
                    }
                } else {
                    Authority::Classic
                };
                (
                    user,
                    Some(token.id),
                    token.scope == "write",
                    token.expires_at,
                    authority,
                )
            }
            Proof::Session { home, value } => {
                if !pool.owns_prefix(*home) {
                    return Ok(None);
                }
                let Some(session) =
                    codeyard_db_identity::users::sessions::get_session(pool, value).await?
                else {
                    return Ok(None);
                };
                let Some(user) =
                    codeyard_db_identity::users::sessions::get_session_user(pool, value).await?
                else {
                    return Ok(None);
                };
                let Some(csrf) = codeyard_db_identity::users::sessions::get_or_create_csrf_token(
                    pool,
                    value,
                    Some(user.id),
                    &codeyard_support::crypto::random_hex(),
                    3600,
                )
                .await?
                else {
                    return Ok(None);
                };
                (
                    user,
                    None,
                    true,
                    Some(session.expires_at),
                    Authority::Session { csrf },
                )
            }
            Proof::Ssh { fingerprint } => {
                let Some(identity) =
                    codeyard_db_identity::users::ssh_keys::get_user_by_key_fingerprint(
                        pool,
                        fingerprint,
                    )
                    .await?
                else {
                    return Ok(None);
                };
                let Some(identity) = codeyard_db_identity::users::ssh_keys::get_user_by_ssh_key_id(
                    pool,
                    identity.key.id,
                )
                .await?
                .filter(|identity| identity.key.fingerprint == *fingerprint) else {
                    return Ok(None);
                };
                (
                    identity.user,
                    Some(identity.key.id),
                    true,
                    None,
                    Authority::Ssh,
                )
            }
        };
        if user.is_stub
            || user.home_cell != i32::from(pool.cell_id().get())
            || user.is_banned
            || user.must_change_password
        {
            return Ok(None);
        }
        let mut answer = Answer {
            home: pool.cell_id(),
            user,
            credential_id: id,
            write,
            expires_at,
            authority,
        };
        answer.redact();
        Ok(Some(answer))
    }
}

/// Resolve a mutation target once and retain its public metadata as a local foreign-key row.
/// Authority is still checked by the resource's writing transaction.
pub async fn resolve_user(
    pool: &DbPool,
    name: &str,
) -> Result<Option<codeyard_db_core::models::User>> {
    if codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Name, name).is_err() {
        return Ok(None);
    }
    if let Some(user) =
        codeyard_db_identity::users::accounts::get_user_by_username(pool, name).await?
    {
        return Ok(Some(user));
    }
    let Some(home) = pool.foreign_authorization() else {
        return Ok(None);
    };
    let Some(user) =
        tokio::time::timeout(std::time::Duration::from_secs(10), home.user(name)).await??
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        user.username == name && !user.is_stub,
        "Invalid foreign account metadata"
    );
    Ok(retain_user(pool, user)
        .await?
        .filter(|stored| stored.username == name))
}

async fn retain_user(
    pool: &DbPool,
    user: codeyard_db_core::models::User,
) -> Result<Option<codeyard_db_core::models::User>> {
    codeyard_db_core::cells::authorization::validate_public_user(&user)?;
    let source = codeyard_support::cells::Prefix::try_from(i64::from(user.home_cell))?;
    codeyard_db_core::cells::stubs::user(pool, source, &user).await?;
    Ok(
        codeyard_db_identity::users::accounts::get_user_by_id(pool, user.id)
            .await?
            .filter(|stored| stored.home_cell == user.home_cell),
    )
}

pub async fn resolve_user_id(
    pool: &DbPool,
    id: i64,
) -> Result<Option<codeyard_db_core::models::User>> {
    if codeyard_support::cells::Prefix::for_id(id).is_err() {
        return Ok(None);
    }
    if let Some(user) = codeyard_db_identity::users::accounts::get_user_by_id(pool, id).await? {
        return Ok(Some(user));
    }
    let Some(home) = pool.foreign_authorization() else {
        return Ok(None);
    };
    let Some(user) =
        tokio::time::timeout(std::time::Duration::from_secs(10), home.user_id(id)).await??
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        user.id == id && !user.is_stub,
        "Invalid foreign account metadata"
    );
    retain_user(pool, user).await
}
