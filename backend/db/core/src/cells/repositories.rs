//! A fork's source is read under its resource cell's authority; its storage stays private.
use crate::models::{Organization, Repository, User};
use anyhow::{ensure, Result};
use codeyard_model::cells::{Object, ObjectKind};
use codeyard_types::cells::Prefix;
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
    pub credential_id: Option<i64>,
    pub default_branch_oid: Option<String>,
    pub repository: Repository,
    pub owner: Owner,
    pub allow_forking: bool,
}
impl Source {
    pub fn validate(&self, home: Prefix) -> Result<()> {
        ensure!(
            self.default_branch_oid
                .as_deref()
                .is_none_or(codeyard_git::is_full_commit_id),
            "Invalid foreign default branch"
        );
        Prefix::for_id(self.reader_id)?;
        if let Some(id) = self.credential_id {
            Prefix::for_id(id)?;
        }
        validate_public(&self.repository, &self.owner, home)?;
        ensure!(
            serde_json::to_vec(self)?.len() <= 128 * 1024,
            "Foreign repository source exceeds its limit"
        );
        Ok(())
    }
    pub async fn retain(&self, pool: &crate::DbPool) -> Result<()> {
        let home = Prefix::new(u16::try_from(self.repository.home_cell)?);
        self.validate(home)?;
        retain_public(pool, home, &self.repository, &self.owner).await
    }
}

/// The current owner and name of the public repository that moved to an owner in another
/// cell from `owner`/`name`, so a request for the old name can be redirected. A repository
/// created later under the old name ends the redirect. Private repositories are not
/// redirected: this cell cannot check access in their new home.
pub async fn redirect(
    pool: &crate::DbPool,
    owner: &str,
    name: &str,
) -> Result<Option<(String, String)>> {
    const SQL: &str = "SELECT COALESCE(u.username,o.name),r.name FROM repository_redirects d \
        JOIN repositories r ON r.id=d.repo_id \
        LEFT JOIN users u ON r.owner_kind='user' AND u.id=r.owner_user_id \
        LEFT JOIN organizations o ON r.owner_kind='org' AND o.id=r.owner_org_id \
        WHERE d.name_key=lower($2) AND NOT r.is_private AND COALESCE(u.username,o.name) IS NOT NULL \
        AND ((d.owner_kind='user' AND d.owner_id IN (SELECT id FROM users WHERE lower(username)=lower($1))) \
        OR (d.owner_kind='org' AND d.owner_id IN (SELECT id FROM organizations WHERE lower(name)=lower($1)))) \
        AND NOT EXISTS(SELECT 1 FROM repositories x WHERE lower(x.name)=d.name_key \
        AND ((d.owner_kind='user' AND x.owner_user_id=d.owner_id) OR (d.owner_kind='org' AND x.owner_org_id=d.owner_id)))";
    Ok(match pool {
        crate::DbPool::Postgres(pool) => {
            sqlx::query_as(SQL)
                .bind(owner)
                .bind(name)
                .fetch_optional(pool)
                .await?
        }
        crate::DbPool::Sqlite(pool) => {
            sqlx::query_as(SQL)
                .bind(owner)
                .bind(name)
                .fetch_optional(pool)
                .await?
        }
    })
}

/// Repository and owner metadata another cell may store as stubs: no storage location, no
/// owner credentials, and both homed in `home`.
pub fn validate_public(repo: &Repository, owner: &Owner, home: Prefix) -> Result<()> {
    Prefix::for_id(repo.id)?;
    if let Some(parent) = repo.forked_from_id {
        Prefix::for_id(parent)?;
        ensure!(parent != repo.id, "Repository cannot fork itself");
    }
    ensure!(
        repo.home_cell == i32::from(home.get())
            && !repo.is_stub
            && !repo.is_gist
            && repo.disk_path.is_empty()
            && repo.storage_node_id.is_none(),
        "Invalid foreign repository source"
    );
    ensure!(
        codeyard_types::repository_names::valid(&repo.name)
            && codeyard_git::branch_ref(&repo.default_branch).is_some(),
        "Invalid foreign repository coordinates"
    );
    let owner_object = owner.object();
    owner_object.validate()?;
    ensure!(
        (repo.owner_kind == "user"
            && repo.owner_user_id == Some(owner_object.id)
            && repo.owner_org_id.is_none()
            && owner_object.kind == ObjectKind::User)
            || (repo.owner_kind == "org"
                && repo.owner_org_id == Some(owner_object.id)
                && repo.owner_user_id.is_none()
                && owner_object.kind == ObjectKind::Organization),
        "Foreign repository has another owner"
    );
    match owner {
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
    Ok(())
}
/// The caller has validated the metadata against the directory's current home.
pub async fn retain_public(
    pool: &crate::DbPool,
    home: Prefix,
    repo: &Repository,
    owner: &Owner,
) -> Result<()> {
    validate_public(repo, owner, home)?;
    match owner {
        Owner::User(user) => {
            super::stubs::user(pool, home, user).await?;
        }
        Owner::Organization(org) => {
            super::stubs::organization(pool, home, org).await?;
        }
    }
    super::stubs::repository(pool, home, repo).await?;
    Ok(())
}

/// Original authority survives forwarding, storage preparation and the final write transaction.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    pub source: Source,
    pub actor: crate::queries::credentials::Actor,
    credential: String,
    #[serde(skip)]
    native_peer: Option<String>,
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
            native_peer: None,
        };
        admission.proof()?;
        ensure!(
            admission.source.reader_id == admission.actor.user_id
                && admission.source.credential_id == admission.actor.token_id(),
            "Fork reader changed"
        );
        Ok(admission)
    }
    /// Filled by the native RPC listener, never accepted from its JSON body.
    pub fn bind_native_peer(&mut self, peer: &str) -> Result<()> {
        codeyard_support::node_identity::parse_public_key(peer)?;
        self.native_peer = Some(peer.to_owned());
        Ok(())
    }
    pub fn native_peer(&self) -> Option<&str> {
        self.native_peer.as_deref()
    }
    fn proof(&self) -> Result<super::authorization::Proof> {
        retained_proof(&self.actor, &self.credential)
    }
    pub fn snapshot_request(
        &self,
        default_branch_only: bool,
        maximum_bytes: u64,
    ) -> Result<SnapshotRequest> {
        let request = SnapshotRequest {
            repository_id: self.source.repository.id,
            reader_id: self.actor.user_id,
            owner: self.source.owner.name().to_owned(),
            name: self.source.repository.name.clone(),
            proof: self.proof()?,
            default_branch_only,
            branch: None,
            maximum_bytes,
            timeout_seconds: 300,
        };
        request.validate()?;
        Ok(request)
    }
    /// The head cell records this publisher's consent to follow `branch` with the
    /// original credential; repeating it is harmless.
    pub async fn subscribe_pull(
        &self,
        pool: &crate::DbPool,
        base_repo_id: i64,
        number: i32,
        branch: &str,
    ) -> Result<()> {
        let binding = super::pulls::Binding {
            base_repo_id,
            number,
            head_repo_id: self.source.repository.id,
            head_branch: branch.to_owned(),
        };
        binding.validate()?;
        let proof = self.proof()?;
        pool.foreign_authorization()
            .ok_or_else(|| anyhow::anyhow!("Pull head cell is unavailable"))?
            .send_pull(&binding, &proof)
            .await
    }
    pub async fn check(&self, pool: &crate::DbPool) -> Result<Source> {
        let source = self.read(pool).await?;
        ensure!(
            !source.repository.is_private || source.allow_forking,
            codeyard_types::credentials::AccountRestricted
        );
        Ok(source)
    }
    pub async fn read(&self, pool: &crate::DbPool) -> Result<Source> {
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
            .ok_or(codeyard_types::credentials::AccountRestricted)?;
        source.validate(home)?;
        ensure!(
            source.repository.id == self.source.repository.id
                && source.reader_id == self.actor.user_id
                && source.credential_id == self.actor.token_id()
                && source.owner.object() == self.source.owner.object(),
            codeyard_types::credentials::AccountRestricted
        );
        Ok(source)
    }
}

pub fn actor_proof(
    actor: &crate::queries::credentials::Actor,
) -> Result<super::authorization::Proof> {
    use super::authorization::Proof;
    use crate::queries::users::accounts::AccountCredential;
    let proof = match actor.credential() {
        Some(AccountCredential::Token(id, hash) | AccountCredential::ForeignToken(id, hash)) => {
            Proof::Token {
                home: Prefix::for_id(id)?,
                hash: hash.to_owned(),
            }
        }
        Some(AccountCredential::Session(session)) => Proof::Session {
            home: codeyard_types::cells::Secret::cookie(session)?.cell,
            value: session.to_owned(),
        },
        None => anyhow::bail!("Fork requires its original account credential"),
    };
    proof.validate()?;
    Ok(proof)
}
pub fn retained_proof(
    actor: &crate::queries::credentials::Actor,
    credential: &str,
) -> Result<super::authorization::Proof> {
    ensure!(
        credential.len() <= 4096 && !credential.contains(['\r', '\n', '\0']),
        "Invalid fork credential"
    );
    let proof = actor_proof(actor)?;
    ensure!(
        match &proof {
            super::authorization::Proof::Token { hash, .. } =>
                codeyard_support::token_hash::token_hash(credential) == *hash,
            super::authorization::Proof::Session { value, .. } => value == credential,
            super::authorization::Proof::Ssh { .. } => false,
        },
        "Fork credential changed"
    );
    Ok(proof)
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
    pub branch: Option<String>,
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
        ensure!(
            self.branch
                .as_deref()
                .is_none_or(|branch| !self.default_branch_only
                    && codeyard_git::branch_ref(branch).is_some()),
            "Invalid snapshot branch selection"
        );
        Prefix::for_id(self.repository_id)?;
        Prefix::for_id(self.reader_id)?;
        codeyard_model::cells::Key::new(codeyard_model::cells::KeyKind::Name, &self.owner)?;
        ensure!(
            codeyard_types::repository_names::valid(&self.name),
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
                request
                    .branch
                    .as_deref()
                    .is_none_or(|branch| name == &format!("refs/heads/{branch}"))
                    && (!request.default_branch_only
                        || name == &format!("refs/heads/{}", self.default_branch)),
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
            branch: None,
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
        let mut branch = request.clone();
        branch.branch = Some("main".into());
        assert!(branch.validate().is_err());
        branch.default_branch_only = false;
        branch.validate().unwrap();
        snapshot.validate(&branch).unwrap();
        branch.branch = Some("other".into());
        assert!(snapshot.validate(&branch).is_err());
        branch.branch = Some("../invalid".into());
        assert!(branch.validate().is_err());
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

/// A fork POST keeps its original HTTP authentication and CSRF checks at the destination.
#[derive(Clone)]
pub struct ForwardFork {
    pub target_kind: ObjectKind,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl ForwardFork {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.target_kind,
                ObjectKind::User | ObjectKind::Organization
            ),
            "Invalid fork destination kind"
        );
        let parts: Vec<_> = self.path.trim_start_matches('/').split('/').collect();
        let allowed = matches!(
            parts.as_slice(),
            ["api", "v3", "repos", _, _, "forks"]
                | ["api", "repos", _, _, "forks"]
                | [_, _, "_forms", "fork"]
        );
        ensure!(
            allowed
                && self.path.starts_with('/')
                && !self.path.starts_with("//")
                && self.path.len() <= 16384
                && !self.path.contains(['?', '#', '\r', '\n', '\0'])
                && !parts
                    .iter()
                    .any(|part| part.is_empty() || *part == "." || *part == "..")
                && self.body.len() <= 65536
                && self.headers.len() <= 32,
            "Invalid fork forwarding request"
        );
        let mut bytes = 0_usize;
        for (name, value) in &self.headers {
            bytes = bytes
                .checked_add(name.len() + value.len())
                .ok_or_else(|| anyhow::anyhow!("Fork headers exceed their limit"))?;
            ensure!(
                bytes <= 32768
                    && request_header(name)
                    && !value
                        .bytes()
                        .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n'),
                "Invalid fork forwarding headers"
            );
        }
        Ok(())
    }
}
pub fn request_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "cookie"
            | "x-csrf-token"
            | "origin"
            | "referer"
            | "sec-fetch-site"
            | "host"
            | "content-type"
            | "accept"
            | "via"
            | "user-agent"
    )
}
pub struct ForwardedFork {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(test)]
mod forward_tests {
    use super::*;
    #[test]
    fn fork_forwarding_bounds_posts_and_preserves_only_request_headers() {
        let mut request = ForwardFork {
            target_kind: ObjectKind::User,
            path: "/api/v3/repos/alice/source/forks".into(),
            headers: vec![("authorization".into(), "Bearer secret".into())],
            body: b"null".to_vec(),
        };
        request.validate().unwrap();
        for path in [
            "https://outside.test/x",
            "//outside.test/x",
            "/admin",
            "/api/v3/repos/a/b/forks?next=bad",
            "/a/b/_forms/delete",
        ] {
            request.path = path.into();
            assert!(request.validate().is_err());
        }
        request.path = "/alice/source/_forms/fork".into();
        request.validate().unwrap();
        request
            .headers
            .push(("x-codeyard-user-id".into(), "1".into()));
        assert!(request.validate().is_err());
        request.headers.pop();
        request.body = vec![0; 65537];
        assert!(request.validate().is_err());
    }
}
