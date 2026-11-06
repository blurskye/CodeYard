//! A fork's source is read under its resource cell's authority; its storage stays private.
use crate::models::{Organization, Repository, User};
use anyhow::{ensure, Result};
use codeyard_model::cells::{Object, ObjectKind};
use codeyard_support::cells::Prefix;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "account",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Owner {
    User(Box<User>),
    Organization(Box<Organization>),
}
impl Owner {
    pub fn object(&self) -> Object {
        match self {
            Self::User(user) => Object {
                kind: ObjectKind::User,
                id: user.id,
            },
            Self::Organization(org) => Object {
                kind: ObjectKind::Organization,
                id: org.id,
            },
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::User(user) => &user.username,
            Self::Organization(org) => &org.name,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub reader_id: i64,
    pub repository: Repository,
    pub owner: Owner,
    pub allow_forking: bool,
    // Only the directory-backed client supplies a URL, never the answering cell's JSON.
    #[serde(skip)]
    pub git_url: Option<String>,
}
impl Source {
    pub fn validate(&self, home: Prefix) -> Result<()> {
        let repo = &self.repository;
        Prefix::for_id(repo.id)?;
        Prefix::for_id(self.reader_id)?;
        ensure!(
            repo.home_cell == i32::from(home.get())
                && !repo.is_stub
                && !repo.is_gist
                && repo.disk_path.is_empty()
                && repo.storage_node_id.is_none(),
            "Invalid foreign repository source"
        );
        ensure!(
            codeyard_support::repository_names::valid(&repo.name)
                && codeyard_git::branch_ref(&repo.default_branch).is_some(),
            "Invalid foreign repository coordinates"
        );
        let owner = self.owner.object();
        owner.validate()?;
        ensure!(
            (repo.owner_kind == "user"
                && repo.owner_user_id == Some(owner.id)
                && repo.owner_org_id.is_none()
                && owner.kind == ObjectKind::User)
                || (repo.owner_kind == "org"
                    && repo.owner_org_id == Some(owner.id)
                    && repo.owner_user_id.is_none()
                    && owner.kind == ObjectKind::Organization),
            "Foreign repository has another owner"
        );
        match &self.owner {
            Owner::User(user) => {
                super::authorization::validate_public_user(user)?;
                ensure!(
                    !user.is_stub && user.home_cell == i32::from(home.get()),
                    "Foreign owner has another home"
                );
            }
            Owner::Organization(org) => ensure!(
                !org.is_stub
                    && org.home_cell == i32::from(home.get())
                    && org.billing_email.is_none()
                    && org.avatar_file_id.is_none(),
                "Foreign owner includes private metadata"
            ),
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 128 * 1024,
            "Foreign repository source exceeds its limit"
        );
        Ok(())
    }
    pub async fn retain(&self, pool: &crate::DbPool) -> Result<()> {
        let home = Prefix::new(u16::try_from(self.repository.home_cell)?);
        self.validate(home)?;
        match &self.owner {
            Owner::User(user) => {
                super::stubs::user(pool, home, user).await?;
            }
            Owner::Organization(org) => {
                super::stubs::organization(pool, home, org).await?;
            }
        }
        super::stubs::repository(pool, home, &self.repository).await?;
        Ok(())
    }
}

/// Original authority survives forwarding, storage preparation and the final write transaction.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    pub source: Source,
    pub actor: crate::queries::credentials::Actor,
    credential: String,
}
impl std::fmt::Debug for Admission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Fork admission ([redacted])")
    }
}
impl Admission {
    pub fn new(
        source: Source,
        actor: crate::queries::credentials::Actor,
        credential: String,
    ) -> Result<Self> {
        let admission = Self {
            source,
            actor,
            credential,
        };
        admission.proof()?;
        ensure!(
            admission.source.reader_id == admission.actor.user_id,
            "Fork reader changed"
        );
        Ok(admission)
    }
    fn proof(&self) -> Result<super::authorization::Proof> {
        use super::authorization::Proof;
        use crate::queries::users::accounts::AccountCredential;
        ensure!(
            self.credential.len() <= 4096 && !self.credential.contains(['\r', '\n', '\0']),
            "Invalid fork credential"
        );
        Ok(match self.actor.credential() {
            Some(
                AccountCredential::Token(id, hash) | AccountCredential::ForeignToken(id, hash),
            ) => {
                ensure!(
                    codeyard_support::token_hash::token_hash(&self.credential) == hash,
                    "Fork credential changed"
                );
                Proof::Token {
                    home: Prefix::for_id(id)?,
                    hash: hash.to_owned(),
                }
            }
            Some(AccountCredential::Session(session)) => {
                ensure!(session == self.credential, "Fork credential changed");
                Proof::Session {
                    home: codeyard_support::cells::Secret::cookie(session)?.cell,
                    value: session.to_owned(),
                }
            }
            None => anyhow::bail!("Fork requires its original account credential"),
        })
    }
    pub fn header(&self) -> Result<(&'static str, String)> {
        Ok(match self.proof()? {
            super::authorization::Proof::Token { .. } => {
                ("Authorization", format!("Bearer {}", self.credential))
            }
            super::authorization::Proof::Session { .. } => {
                ("Cookie", format!("session_id={}", self.credential))
            }
            super::authorization::Proof::Ssh { .. } => unreachable!(),
        })
    }
    pub async fn check(&self, pool: &crate::DbPool) -> Result<Source> {
        let home = Prefix::new(u16::try_from(self.source.repository.home_cell)?);
        self.source.validate(home)?;
        ensure!(home != pool.cell_id(), "Fork source is native");
        let proof = self.proof()?;
        let provider = pool
            .foreign_authorization()
            .ok_or_else(|| anyhow::anyhow!("Fork source is unavailable"))?;
        let source = provider
            .repository_source(
                self.source.owner.name(),
                &self.source.repository.name,
                &proof,
            )
            .await?
            .ok_or(codeyard_support::credentials::AccountRestricted)?;
        source.validate(home)?;
        ensure!(
            source.repository.id == self.source.repository.id
                && source.reader_id == self.actor.user_id
                && source.owner.object() == self.source.owner.object()
                && (!source.repository.is_private || source.allow_forking),
            codeyard_support::credentials::AccountRestricted
        );
        Ok(source)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub repository_id: i64,
    pub reader_id: i64,
    pub owner: String,
    pub name: String,
    pub proof: super::authorization::Proof,
    pub default_branch_only: bool,
    pub maximum_bytes: u64,
    pub timeout_seconds: u64,
}
impl SnapshotRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64 * 1024 * 1024 * 1024).contains(&self.maximum_bytes)
                && (1..=300).contains(&self.timeout_seconds),
            "Invalid fork snapshot limits"
        );
        Prefix::for_id(self.repository_id)?;
        Prefix::for_id(self.reader_id)?;
        codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Name, &self.owner)?;
        ensure!(
            codeyard_support::repository_names::valid(&self.name),
            "Invalid fork source name"
        );
        self.proof.validate()
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub repository_id: i64,
    pub reader_id: i64,
    pub default_branch: String,
    pub refs: std::collections::HashMap<String, String>,
}
impl Snapshot {
    pub fn validate(&self, request: &SnapshotRequest) -> Result<()> {
        ensure!(
            self.repository_id == request.repository_id && self.reader_id == request.reader_id,
            "Fork snapshot has another source or reader"
        );
        ensure!(
            codeyard_git::branch_ref(&self.default_branch).is_some()
                && self.refs.len() <= codeyard_git::MAX_REPOSITORY_REFS,
            "Invalid fork snapshot references"
        );
        let mut bytes = 0_usize;
        for (name, target) in &self.refs {
            bytes = bytes
                .checked_add(name.len())
                .ok_or_else(|| anyhow::anyhow!("Fork snapshot references exceed their limit"))?;
            ensure!(
                bytes <= codeyard_git::MAX_REPOSITORY_REF_BYTES
                    && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
                    && codeyard_git::is_canonical_ref(name)
                    && codeyard_git::is_full_commit_id(target),
                "Invalid fork snapshot reference"
            );
            ensure!(
                !request.default_branch_only
                    || name == &format!("refs/heads/{}", self.default_branch),
                "Fork snapshot includes unrequested references"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn snapshots_bind_the_reader_root_and_requested_refs() {
        let request = SnapshotRequest {
            repository_id: 1,
            reader_id: 2,
            owner: "owner".into(),
            name: "source".into(),
            proof: super::super::authorization::Proof::Token {
                home: Prefix::ZERO,
                hash: "a".repeat(64),
            },
            default_branch_only: true,
            maximum_bytes: 1024,
            timeout_seconds: 10,
        };
        request.validate().unwrap();
        let mut snapshot = Snapshot {
            repository_id: 1,
            reader_id: 2,
            default_branch: "main".into(),
            refs: std::collections::HashMap::from([("refs/heads/main".into(), "a".repeat(40))]),
        };
        snapshot.validate(&request).unwrap();
        snapshot.reader_id = 3;
        assert!(snapshot.validate(&request).is_err());
        snapshot.reader_id = 2;
        snapshot.repository_id = 3;
        assert!(snapshot.validate(&request).is_err());
        snapshot.repository_id = 1;
        snapshot
            .refs
            .insert("refs/tags/extra".into(), "a".repeat(40));
        assert!(snapshot.validate(&request).is_err());
        snapshot.refs.remove("refs/tags/extra");
        snapshot
            .refs
            .insert("refs/heads/main".into(), "not-an-object".into());
        assert!(snapshot.validate(&request).is_err());
    }
}
