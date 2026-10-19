//! Push permission checks in the receive hooks, on a throwaway SQLite database.
pub(in crate::backend::repositories::git_sessions) use fixture::{
    before_authorize, hold_next_hook,
};

#[cfg(test)]
mod fixture {
    use super::super::*;
    use crate::backend::db::models::AccessLevel;
    use crate::backend::repositories::git_sessions::validation::ZERO_SHA;
    use base64::Engine;
    use std::sync::LazyLock;
    use tokio::sync::oneshot;

    struct Barrier {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }
    static BARRIERS: LazyLock<Mutex<HashMap<(PathBuf, bool), Barrier>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(in crate::backend::repositories::git_sessions) async fn before_authorize(
        operation: &Operation,
        post: bool,
    ) {
        let barrier = BARRIERS
            .lock()
            .unwrap()
            .remove(&(operation.path.clone(), post));
        if let Some(barrier) = barrier {
            barrier.entered.send(()).unwrap();
            barrier.release.await.unwrap();
        }
    }

    /// Holds the next hook of this kind for the repository at `path` as it enters.
    /// `reached` resolves once it is held, and sending on `release` lets it continue.
    pub(in crate::backend::repositories::git_sessions) fn hold_next_hook(
        path: PathBuf,
        post: bool,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, reached) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        BARRIERS.lock().unwrap().insert(
            (path, post),
            Barrier {
                entered,
                release: wait,
            },
        );
        (reached, release)
    }

    /// Waits until the push reaches its hook. If the push ends first, say why
    /// instead of timing out.
    async fn hook_reached(
        reached: oneshot::Receiver<()>,
        task: &mut tokio::task::JoinHandle<Result<Vec<u8>>>,
    ) {
        tokio::select! {
            reached = timeout(std::time::Duration::from_secs(15), reached) => {
                reached.unwrap().unwrap();
            }
            finished = &mut *task => {
                panic!("the push ended before its hook ran: {:?}", finished.unwrap());
            }
        }
    }

    #[derive(Clone, Copy)]
    enum CredentialKind {
        SshKey,
        Token,
    }

    #[derive(Clone, Copy)]
    enum AccessKind {
        Direct,
        OrganizationTeam,
    }

    struct Fixture {
        _directory: TempDir,
        path: PathBuf,
        pool: DbPool,
        native: sqlx::SqlitePool,
        ci: Arc<CiRuns>,
        repo_id: i64,
        owner_id: i64,
        actor_id: i64,
        key_id: i64,
        token_id: i64,
        credential: ReceiveCredential,
        team: Option<(i64, i64)>,
        request: Vec<u8>,
        oid: git2::Oid,
    }
    impl Fixture {
        async fn new() -> Self {
            Self::with_authority(CredentialKind::SshKey, AccessKind::Direct).await
        }

        async fn with_authority(credential: CredentialKind, access: AccessKind) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("repository.git");
            let native = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(4)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(directory.path().join("application.sqlite"))
                        .create_if_missing(true)
                        .foreign_keys(true),
                )
                .await
                .unwrap();
            let pool = DbPool::from(native.clone());
            db::run_migrations(&pool).await.unwrap();
            let owner = db::users::accounts::create_user(&pool, "owner", None)
                .await
                .unwrap();
            let actor = db::users::accounts::create_user(&pool, "writer", None)
                .await
                .unwrap();
            let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
            wire.extend_from_slice(&[7; 32]);
            let key = format!(
                "ssh-ed25519 {}",
                base64::engine::general_purpose::STANDARD.encode(wire)
            );
            let key = db::users::ssh_keys::add_ssh_key(&pool, actor.id, "writer", &key)
                .await
                .unwrap();
            let token_hash = "a".repeat(64);
            let session = "b".repeat(64);
            db::users::sessions::create_session(
                &pool,
                &session,
                actor.id,
                None,
                None,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
            let token = db::users::tokens::create_token(
                &pool,
                actor.id,
                &session,
                "writer",
                &token_hash,
                "write",
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let (organization, team) = match access {
                AccessKind::Direct => (None, None),
                AccessKind::OrganizationTeam => {
                    let organization = match db::organizations::create_organization(
                        &pool,
                        owner.id,
                        &db::organizations::NewOrganization::new(
                            "fixture_org",
                            db::organizations::OrganizationProfile::new(None, None).unwrap(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(organization) => {
                            organization
                        }
                        outcome => panic!("organization setup failed: {outcome:?}"),
                    };
                    match db::organizations::add_organization_member(
                        &pool,
                        owner.id,
                        organization.id,
                        actor.id,
                        db::models::OrgRole::Member,
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(_) => {}
                        outcome => panic!("organization membership setup failed: {outcome:?}"),
                    }
                    let team = match db::organizations::create_organization_team(
                        &pool,
                        owner.id,
                        organization.id,
                        &db::organizations::NewTeam::new(
                            "writers",
                            db::organizations::TeamProfile::new(
                                "Writers",
                                None,
                                db::organizations::TeamPrivacy::Visible,
                            )
                            .unwrap(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(team) => team,
                        outcome => panic!("team setup failed: {outcome:?}"),
                    };
                    match db::organizations::add_organization_team_member(
                        &pool,
                        owner.id,
                        organization.id,
                        team.id,
                        actor.id,
                        db::organizations::TeamRole::Member,
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(_) => {}
                        outcome => panic!("team membership setup failed: {outcome:?}"),
                    }
                    (Some(organization.id), Some((organization.id, team.id)))
                }
            };
            let bare = git2::Repository::init_bare(&path).unwrap();
            let path = std::fs::canonicalize(path).unwrap();
            let tree_oid = bare.treebuilder(None).unwrap().write().unwrap();
            let tree = bare.find_tree(tree_oid).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            let oid = bare
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "fixture",
                    &tree,
                    &[],
                )
                .unwrap();
            let repo = db::repositories::records::create_repository(
                &pool,
                owner.id,
                "repo",
                None,
                true,
                path.to_str().unwrap(),
            )
            .await
            .unwrap();
            if let Some(organization) = organization {
                sqlx::query(
                    "UPDATE repositories SET owner_kind='org', owner_user_id=NULL, owner_org_id=? WHERE id=?",
                )
                .bind(organization)
                .bind(repo.id)
                .execute(&native)
                .await
                .unwrap();
            }
            match team {
                None => {
                    db::repositories::access::grant_repo_access(
                        &pool,
                        actor.id,
                        repo.id,
                        AccessLevel::Write,
                    )
                    .await
                    .unwrap();
                }
                Some((organization, team)) => {
                    match db::organizations::grant_organization_team_repository(
                        &pool,
                        owner.id,
                        organization,
                        team,
                        repo.id,
                        db::organizations::TeamRepositoryPermission::new(3).unwrap(),
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(_) => {}
                        outcome => panic!("team repository setup failed: {outcome:?}"),
                    }
                }
            }
            let ci = Arc::new(CiRuns::new(
                pool.clone(),
                Arc::new(crate::backend::ci::CiConfig::default()),
                Arc::new(tokio::sync::Notify::new()),
                None,
                Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                    &pool,
                )),
            ));
            let command = format!("{oid} {ZERO_SHA} refs/heads/main\0report-status\n");
            let request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
            Self {
                _directory: directory,
                path,
                pool,
                native,
                ci,
                repo_id: repo.id,
                owner_id: owner.id,
                actor_id: actor.id,
                key_id: key.id,
                token_id: token.id,
                credential: match credential {
                    CredentialKind::SshKey => ReceiveCredential::SshKey(key.id),
                    CredentialKind::Token => ReceiveCredential::Token(token.id),
                },
                team,
                request,
                oid,
            }
        }

        /// Makes the push delete a branch that is not the default one.
        fn delete_spare_branch(&mut self) {
            let bare = git2::Repository::open_bare(&self.path).unwrap();
            bare.reference("refs/heads/spare", self.oid, false, "spare")
                .unwrap();
            let command = format!("{} {ZERO_SHA} refs/heads/spare\0report-status\n", self.oid);
            self.request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
        }

        /// Leaves `topic` as the only branch while HEAD and the stored default still name the
        /// absent `main`, and makes the push delete `topic`.
        fn delete_sole_topic_branch(&mut self) {
            let bare = git2::Repository::open_bare(&self.path).unwrap();
            bare.reference("refs/heads/topic", self.oid, false, "topic")
                .unwrap();
            bare.find_reference("refs/heads/main")
                .unwrap()
                .delete()
                .unwrap();
            let command = format!("{} {ZERO_SHA} refs/heads/topic\0report-status\n", self.oid);
            self.request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
        }

        fn receive(&self) -> tokio::task::JoinHandle<Result<Vec<u8>>> {
            let pool = self.pool.clone();
            let ci = self.ci.clone();
            let path = self.path.clone();
            let request = self.request.clone();
            let repo_id = self.repo_id;
            let credential = self.credential;
            tokio::spawn(async move {
                let mut session = GitSession::new(GitService::ReceivePack, true);
                session.credential = Some(credential);
                session.owner = "owner".into();
                let mut output = Vec::new();
                let status = receive_session(
                    pool,
                    ci,
                    super::super::super::ReceiveSession {
                        repo_id,
                        path: &path,
                        session,
                    },
                    GitStreams {
                        input: &mut request.as_slice(),
                        output: &mut output,
                        errors: &mut tokio::io::sink(),
                    },
                )
                .await?;
                ensure!(status.success(), "Git receive failed");
                Ok(output)
            })
        }
    }

    #[tokio::test]
    async fn push_rechecks_revocation_and_protection_in_pre_receive() {
        for change in [
            "unchanged",
            "key",
            "token",
            "ban",
            "reset",
            "access",
            "organization-team",
            "archive",
            "protected",
        ] {
            let mut fixture = match change {
                "token" => Fixture::with_authority(CredentialKind::Token, AccessKind::Direct).await,
                "organization-team" => {
                    Fixture::with_authority(CredentialKind::SshKey, AccessKind::OrganizationTeam)
                        .await
                }
                _ => Fixture::new().await,
            };
            fixture.delete_spare_branch();
            let (entered, reached) = oneshot::channel();
            let (release, wait) = oneshot::channel();
            BARRIERS.lock().unwrap().insert(
                (fixture.path.clone(), false),
                Barrier {
                    entered,
                    release: wait,
                },
            );
            let mut task = fixture.receive();
            hook_reached(reached, &mut task).await;
            // Both session/command admission and native unpacking have completed.
            // Mutate live authority while the real pre-receive helper is waiting.
            match change {
                "unchanged" => {}
                "key" => {
                    assert!(
                        db::users::ssh_keys::delete_ssh_key(&fixture.pool, fixture.key_id)
                            .await
                            .unwrap()
                    );
                }
                "token" => {
                    assert!(db::users::tokens::delete_token(
                        &fixture.pool,
                        fixture.actor_id,
                        fixture.token_id,
                    )
                    .await
                    .unwrap());
                }
                "ban" => {
                    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=?")
                        .bind(fixture.actor_id)
                        .execute(&fixture.native)
                        .await
                        .unwrap();
                }
                "reset" => {
                    db::users::accounts::update_user_password(
                        &fixture.pool,
                        fixture.actor_id,
                        "fixture-only",
                        true,
                    )
                    .await
                    .unwrap();
                }
                "access" => {
                    assert!(db::repositories::access::revoke_repo_access(
                        &fixture.pool,
                        fixture.actor_id,
                        fixture.repo_id
                    )
                    .await
                    .unwrap());
                }
                "organization-team" => {
                    let (organization, team) = fixture.team.unwrap();
                    match db::organizations::remove_organization_team_member(
                        &fixture.pool,
                        fixture.owner_id,
                        organization,
                        team,
                        fixture.actor_id,
                    )
                    .await
                    .unwrap()
                    {
                        db::organizations::OrganizationOutcome::Applied(_) => {}
                        outcome => panic!("team authority removal failed: {outcome:?}"),
                    }
                }
                "archive" => {
                    sqlx::query("UPDATE repositories SET is_archived=TRUE WHERE id=?")
                        .bind(fixture.repo_id)
                        .execute(&fixture.native)
                        .await
                        .unwrap();
                }
                "protected" => {
                    db::repositories::protection::upsert_branch_protection_rule(
                        &fixture.pool,
                        db::repositories::BranchProtectionRuleUpsert {
                            actor_id: fixture.owner_id,
                            repo_id: fixture.repo_id,
                            pattern: "spare",
                            require_pull_request: true,
                            required_approving_reviews: 1,
                            dismiss_stale_reviews: true,
                            require_status_checks: false,
                            required_status_checks: vec![],
                            require_up_to_date_branch: false,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                }
                _ => unreachable!(),
            }
            release.send(()).unwrap();
            let output = timeout(std::time::Duration::from_secs(15), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let output = String::from_utf8_lossy(&output);
            let bare = git2::Repository::open_bare(&fixture.path).unwrap();
            if change == "unchanged" {
                assert!(output.contains("ok refs/heads/spare"), "{change}: {output}");
                assert!(bare.find_reference("refs/heads/spare").is_err());
            } else {
                assert!(output.contains("ng refs/heads/spare"), "{change}: {output}");
                assert_eq!(
                    bare.find_reference("refs/heads/spare").unwrap().target(),
                    Some(fixture.oid),
                    "{change}"
                );
            }
            assert_eq!(
                bare.find_reference("refs/heads/main").unwrap().target(),
                Some(fixture.oid),
                "{change}"
            );
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn receive_session_ignores_repository_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new().await;
        let hook = fixture.path.join("hooks/pre-receive");
        std::fs::write(&hook, b"#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        fixture.delete_spare_branch();
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/spare"));
        assert!(bare.find_reference("refs/heads/spare").is_err());
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn push_cannot_delete_the_default_branch() {
        // The fixture's own request deletes `main`, which is the default branch.
        let fixture = Fixture::new().await;
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ng refs/heads/main"));
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        assert_eq!(
            bare.find_reference("refs/heads/main").unwrap().target(),
            Some(fixture.oid)
        );
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn push_cannot_delete_the_sole_branch_git_advertises() {
        // HEAD and the stored default still name the absent `main`, so Git advertises `topic`
        // as the default. The stored name is `main`, so only the advertised default guards it.
        let mut fixture = Fixture::new().await;
        fixture.delete_sole_topic_branch();
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ng refs/heads/topic"));
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        assert_eq!(
            bare.find_reference("refs/heads/topic").unwrap().target(),
            Some(fixture.oid)
        );
        fixture.pool.close().await;
    }

    fn receive_request(path: &Path, reference: &str, old: &str, new: &str) -> Vec<u8> {
        receive_requests(path, &[(reference, old, new)])
    }

    /// One receive request that carries every `(reference, old, new)` command and the objects
    /// of the new tips.
    fn receive_requests(path: &Path, updates: &[(&str, &str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut request = Vec::new();
        for (index, (reference, old, new)) in updates.iter().enumerate() {
            // Git reads the capabilities from the first command only.
            let capabilities = if index == 0 { "\0report-status" } else { "" };
            let command = format!("{old} {new} {reference}{capabilities}\n");
            request.extend_from_slice(format!("{:04x}{command}", command.len() + 4).as_bytes());
        }
        request.extend_from_slice(b"0000");
        if updates.iter().any(|(_, _, new)| *new != ZERO_SHA) {
            let mut pack = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["pack-objects", "--stdout", "--revs"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = pack.stdin.take().unwrap();
            for (_, old, new) in updates {
                if *new != ZERO_SHA {
                    writeln!(input, "{new}").unwrap();
                    if *old != ZERO_SHA {
                        writeln!(input, "^{old}").unwrap();
                    }
                }
            }
            drop(input);
            let output = pack.wait_with_output().unwrap();
            assert!(output.status.success());
            request.extend_from_slice(&output.stdout);
        }
        request
    }

    /// A bare clone of the fixture repository where a test builds the commits it then pushes,
    /// so that the pushed objects reach the repository only through the receive quarantine.
    struct Scratch {
        repository: git2::Repository,
    }
    impl Scratch {
        fn new(fixture: &Fixture) -> Self {
            let path = fixture._directory.path().join("scratch.git");
            let status = std::process::Command::new("git")
                .arg("clone")
                .arg("--bare")
                .arg("--quiet")
                .arg(&fixture.path)
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            Self {
                repository: git2::Repository::open_bare(path).unwrap(),
            }
        }

        fn commit(&self, parents: &[git2::Oid], message: &str) -> git2::Oid {
            let tree = self
                .repository
                .find_tree(self.repository.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|parent| self.repository.find_commit(*parent).unwrap())
                .collect();
            let parents: Vec<_> = parents.iter().collect();
            self.repository
                .commit(None, &signature, &signature, message, &tree, &parents)
                .unwrap()
        }
    }

    impl Fixture {
        /// Stores a rule for `pattern` that allows direct writes unless `change` says otherwise.
        async fn protect(
            &self,
            pattern: &str,
            change: impl FnOnce(&mut db::repositories::BranchProtectionRuleUpsert<'_>),
        ) {
            let mut rule = db::repositories::BranchProtectionRuleUpsert {
                actor_id: self.owner_id,
                repo_id: self.repo_id,
                pattern,
                require_pull_request: false,
                ..Default::default()
            };
            change(&mut rule);
            db::repositories::protection::upsert_branch_protection_rule(&self.pool, rule)
                .await
                .unwrap();
        }

        /// Pushes `old` to `new` on `reference` as the writer and returns Git's report.
        async fn push(
            &mut self,
            scratch: &Scratch,
            reference: &str,
            old: &str,
            new: &str,
        ) -> String {
            self.push_all(scratch, &[(reference, old, new)]).await
        }

        /// Pushes every `(reference, old, new)` in one receive and returns Git's report.
        async fn push_all(&mut self, scratch: &Scratch, updates: &[(&str, &str, &str)]) -> String {
            self.request = receive_requests(scratch.repository.path(), updates);
            let output = timeout(std::time::Duration::from_secs(30), self.receive())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            String::from_utf8_lossy(&output).into_owned()
        }

        fn tip(&self, reference: &str) -> Option<git2::Oid> {
            git2::Repository::open_bare(&self.path)
                .unwrap()
                .find_reference(reference)
                .ok()
                .and_then(|reference| reference.target())
        }
    }

    #[tokio::test]
    async fn force_push_is_refused_unless_the_rule_allows_it() {
        for allowed in [false, true] {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            let first = scratch.commit(&[fixture.oid], "first");
            let rewritten = scratch.commit(&[fixture.oid], "rewritten");
            let reference = "refs/heads/topic";
            let output = fixture
                .push(&scratch, reference, ZERO_SHA, &first.to_string())
                .await;
            assert!(output.contains("ok refs/heads/topic"), "{output}");
            fixture
                .protect("topic", |rule| rule.block_force_pushes = !allowed)
                .await;

            let output = fixture
                .push(
                    &scratch,
                    reference,
                    &first.to_string(),
                    &rewritten.to_string(),
                )
                .await;
            if allowed {
                assert!(output.contains("ok refs/heads/topic"), "{output}");
                assert_eq!(fixture.tip(reference), Some(rewritten));
            } else {
                assert!(output.contains("ng refs/heads/topic"), "{output}");
                assert_eq!(fixture.tip(reference), Some(first));
            }

            // A fast-forward is never a force push.
            let from = fixture.tip(reference).unwrap();
            let child = scratch.commit(&[from], "child");
            let output = fixture
                .push(&scratch, reference, &from.to_string(), &child.to_string())
                .await;
            assert!(output.contains("ok refs/heads/topic"), "{output}");
            assert_eq!(fixture.tip(reference), Some(child));
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn deleting_a_branch_is_refused_unless_the_rule_allows_it() {
        for allowed in [false, true] {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            fixture.delete_spare_branch();
            let spare = fixture.oid.to_string();
            fixture
                .protect("spare", |rule| rule.block_deletions = !allowed)
                .await;
            let output = fixture
                .push(&scratch, "refs/heads/spare", &spare, ZERO_SHA)
                .await;
            if allowed {
                assert!(output.contains("ok refs/heads/spare"), "{output}");
                assert_eq!(fixture.tip("refs/heads/spare"), None);
            } else {
                assert!(output.contains("ng refs/heads/spare"), "{output}");
                assert_eq!(fixture.tip("refs/heads/spare"), Some(fixture.oid));
            }
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn restricted_pushes_allow_only_the_listed_users() {
        for listed in [false, true] {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            let allowed = if listed {
                fixture.actor_id
            } else {
                fixture.owner_id
            };
            fixture
                .protect("topic", |rule| {
                    rule.restrict_pushes = true;
                    rule.push_allowances = vec![allowed];
                })
                .await;
            let commit = scratch.commit(&[fixture.oid], "topic");
            let output = fixture
                .push(&scratch, "refs/heads/topic", ZERO_SHA, &commit.to_string())
                .await;
            if listed {
                assert!(output.contains("ok refs/heads/topic"), "{output}");
                assert_eq!(fixture.tip("refs/heads/topic"), Some(commit));
            } else {
                assert!(output.contains("ng refs/heads/topic"), "{output}");
                assert_eq!(fixture.tip("refs/heads/topic"), None);
            }
            fixture.pool.close().await;
        }
    }

    /// Git judges a push as one request, so a protected branch in it refuses the whole push.
    /// The unprotected branch lands once it is pushed alone.
    #[tokio::test]
    async fn multi_ref_push_is_refused_when_one_branch_is_protected() {
        let mut fixture = Fixture::new().await;
        let scratch = Scratch::new(&fixture);
        let guarded = scratch.commit(&[fixture.oid], "guarded");
        let free = scratch.commit(&[fixture.oid], "free");
        let (guarded_new, free_new) = (guarded.to_string(), free.to_string());
        fixture
            .protect("guarded", |rule| rule.require_pull_request = true)
            .await;
        let output = fixture
            .push_all(
                &scratch,
                &[
                    ("refs/heads/guarded", ZERO_SHA, guarded_new.as_str()),
                    ("refs/heads/free", ZERO_SHA, free_new.as_str()),
                ],
            )
            .await;
        assert!(output.contains("ng refs/heads/guarded"), "{output}");
        assert_eq!(fixture.tip("refs/heads/guarded"), None);
        assert_eq!(fixture.tip("refs/heads/free"), None);
        let output = fixture
            .push(&scratch, "refs/heads/free", ZERO_SHA, &free_new)
            .await;
        assert!(output.contains("ok refs/heads/free"), "{output}");
        assert_eq!(fixture.tip("refs/heads/free"), Some(free));
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn linear_history_refuses_pushed_merge_commits() {
        for required in [true, false] {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            let left = scratch.commit(&[fixture.oid], "left");
            let right = scratch.commit(&[fixture.oid], "right");
            let merge = scratch.commit(&[left, right], "merge");
            let reference = "refs/heads/topic-left";
            let output = fixture
                .push(&scratch, reference, ZERO_SHA, &left.to_string())
                .await;
            assert!(output.contains("ok refs/heads/topic-left"), "{output}");
            fixture
                .protect("topic-*", |rule| rule.require_linear_history = required)
                .await;

            let output = fixture
                .push(&scratch, reference, &left.to_string(), &merge.to_string())
                .await;
            let created = fixture
                .push(
                    &scratch,
                    "refs/heads/topic-new",
                    ZERO_SHA,
                    &merge.to_string(),
                )
                .await;
            if required {
                assert!(output.contains("ng refs/heads/topic-left"), "{output}");
                assert!(created.contains("ng refs/heads/topic-new"), "{created}");
                assert_eq!(fixture.tip(reference), Some(left));
                assert_eq!(fixture.tip("refs/heads/topic-new"), None);
            } else {
                assert!(output.contains("ok refs/heads/topic-left"), "{output}");
                assert!(created.contains("ok refs/heads/topic-new"), "{created}");
                assert_eq!(fixture.tip(reference), Some(merge));
                assert_eq!(fixture.tip("refs/heads/topic-new"), Some(merge));
            }
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn rules_that_need_a_pull_request_block_admins_only_while_enforce_admins_is_on() {
        for (enforce_admins, admin) in [(true, false), (true, true), (false, false), (false, true)]
        {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            if admin {
                db::repositories::access::grant_repo_access(
                    &fixture.pool,
                    fixture.actor_id,
                    fixture.repo_id,
                    AccessLevel::Admin,
                )
                .await
                .unwrap();
            }
            fixture
                .protect("topic", |rule| {
                    rule.require_pull_request = true;
                    rule.enforce_admins = enforce_admins;
                })
                .await;
            let commit = scratch.commit(&[fixture.oid], "topic");
            let output = fixture
                .push(&scratch, "refs/heads/topic", ZERO_SHA, &commit.to_string())
                .await;
            let case = format!("enforce_admins={enforce_admins} admin={admin}: {output}");
            if admin && !enforce_admins {
                assert!(output.contains("ok refs/heads/topic"), "{case}");
                assert_eq!(fixture.tip("refs/heads/topic"), Some(commit), "{case}");
            } else {
                assert!(output.contains("ng refs/heads/topic"), "{case}");
                assert_eq!(fixture.tip("refs/heads/topic"), None, "{case}");
            }
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn administrators_cannot_force_push_where_a_rule_blocks_it() {
        let mut fixture = Fixture::new().await;
        let scratch = Scratch::new(&fixture);
        db::repositories::access::grant_repo_access(
            &fixture.pool,
            fixture.actor_id,
            fixture.repo_id,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
        let first = scratch.commit(&[fixture.oid], "first");
        let rewritten = scratch.commit(&[fixture.oid], "rewritten");
        let output = fixture
            .push(&scratch, "refs/heads/topic", ZERO_SHA, &first.to_string())
            .await;
        assert!(output.contains("ok refs/heads/topic"), "{output}");
        fixture
            .protect("topic", |rule| {
                rule.enforce_admins = false;
                rule.block_force_pushes = true;
            })
            .await;

        let output = fixture
            .push(
                &scratch,
                "refs/heads/topic",
                &first.to_string(),
                &rewritten.to_string(),
            )
            .await;
        assert!(output.contains("ng refs/heads/topic"), "{output}");
        assert_eq!(fixture.tip("refs/heads/topic"), Some(first));
        fixture.pool.close().await;
    }

    /// A rule that blocks force pushes refuses everyone except the users it lists, administrators
    /// included even though `enforce_admins` is off.
    #[tokio::test]
    async fn force_push_allowances_name_who_may_force_push_past_a_rule() {
        for (admin, listed) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut fixture = Fixture::new().await;
            let scratch = Scratch::new(&fixture);
            if admin {
                db::repositories::access::grant_repo_access(
                    &fixture.pool,
                    fixture.actor_id,
                    fixture.repo_id,
                    AccessLevel::Admin,
                )
                .await
                .unwrap();
            }
            let first = scratch.commit(&[fixture.oid], "first");
            let rewritten = scratch.commit(&[fixture.oid], "rewritten");
            let output = fixture
                .push(&scratch, "refs/heads/topic", ZERO_SHA, &first.to_string())
                .await;
            assert!(output.contains("ok refs/heads/topic"), "{output}");
            let actor_id = fixture.actor_id;
            fixture
                .protect("topic", |rule| {
                    rule.block_force_pushes = true;
                    rule.enforce_admins = false;
                    rule.force_push_allowances = if listed { vec![actor_id] } else { vec![] };
                })
                .await;

            let output = fixture
                .push(
                    &scratch,
                    "refs/heads/topic",
                    &first.to_string(),
                    &rewritten.to_string(),
                )
                .await;
            let case = format!("admin={admin} listed={listed}: {output}");
            if listed {
                assert!(output.contains("ok refs/heads/topic"), "{case}");
                assert_eq!(fixture.tip("refs/heads/topic"), Some(rewritten), "{case}");
            } else {
                assert!(output.contains("ng refs/heads/topic"), "{case}");
                assert_eq!(fixture.tip("refs/heads/topic"), Some(first), "{case}");
            }
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn trusted_push_of_a_tag_that_names_a_blob_is_not_a_failure() {
        let mut fixture = Fixture::new().await;
        fixture.ci = Arc::new(CiRuns::new(
            fixture.pool.clone(),
            Arc::new(crate::backend::ci::CiConfig {
                trusted_actor_ids: [fixture.actor_id].into(),
                ..Default::default()
            }),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &fixture.pool,
            )),
        ));
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let blob = bare.blob(b"not a commit").unwrap();
        fixture.request = receive_request(
            &fixture.path,
            "refs/tags/notes",
            ZERO_SHA,
            &blob.to_string(),
        );
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ok refs/tags/notes"));
        assert_eq!(
            bare.find_reference("refs/tags/notes").unwrap().target(),
            Some(blob)
        );
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn untrusted_push_does_not_parse_repository_workflows() {
        let mut fixture = Fixture::new().await;
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let mut workflows = bare.treebuilder(None).unwrap();
        for index in 0..=crate::backend::git::MAX_WORKFLOW_DIRECTORY_ENTRIES {
            let blob = bare.blob(b"malformed: [").unwrap();
            workflows
                .insert(format!("workflow-{index}.yml"), blob, 0o100644)
                .unwrap();
        }
        let workflows = workflows.write().unwrap();
        let mut github = bare.treebuilder(None).unwrap();
        github.insert("workflows", workflows, 0o040000).unwrap();
        let github = github.write().unwrap();
        let mut root = bare.treebuilder(None).unwrap();
        root.insert(".github", github, 0o040000).unwrap();
        let root = bare.find_tree(root.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let parent = bare.find_commit(fixture.oid).unwrap();
        let commit = bare
            .commit(
                None,
                &signature,
                &signature,
                "untrusted push",
                &root,
                &[&parent],
            )
            .unwrap();
        fixture.request = receive_request(
            &fixture.path,
            "refs/heads/main",
            &fixture.oid.to_string(),
            &commit.to_string(),
        );

        // Only a repository that a storage node holds has replicas to tell, and a node
        // keeps its repositories at `<uuid>.git`.
        let key = crate::backend::cluster::NodeKey::generate();
        let node = db::cluster::register_own_node(
            &fixture.pool,
            &key.public_hex(),
            "policy-node",
            &["storage".to_owned()],
            "test",
        )
        .await
        .unwrap();
        let held = fixture
            .path
            .with_file_name(format!("{}.git", uuid::Uuid::new_v4()));
        std::fs::rename(&fixture.path, &held).unwrap();
        sqlx::query("UPDATE repositories SET disk_path = ?1 WHERE id = ?2")
            .bind(held.to_str().unwrap())
            .bind(fixture.repo_id)
            .execute(&fixture.native)
            .await
            .unwrap();
        fixture.path = held;
        assert!(
            db::cluster::place_repository(&fixture.pool, fixture.repo_id, node.id)
                .await
                .unwrap()
        );
        fixture.ci = Arc::new(CiRuns::new(
            fixture.pool.clone(),
            Arc::new(crate::backend::ci::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            Some(crate::backend::cluster::NodeContext {
                pool: Some(fixture.pool.clone()),
                iroh: crate::backend::cluster::IrohDialer::with_endpoint(
                    crate::backend::cluster::Relay::Off,
                    key.clone(),
                    crate::backend::cluster::bind_client(
                        &crate::backend::cluster::Relay::Off,
                        &key,
                    )
                    .await
                    .unwrap(),
                ),
                key: key.clone(),
                node_id: node.id,
                repos_path: fixture.path.parent().unwrap().to_owned(),
                peers: Default::default(),
                unreachable: Default::default(),
            }),
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &fixture.pool,
            )),
        ));
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let version = db::cluster::get_write_version(&fixture.pool, fixture.repo_id)
            .await
            .unwrap()
            .unwrap();
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/main"));
        assert_eq!(
            bare.find_reference("refs/heads/main").unwrap().target(),
            Some(commit)
        );
        // Every transport shares this receive path, so a push is counted for replicas here.
        assert_eq!(
            db::cluster::get_write_version(&fixture.pool, fixture.repo_id)
                .await
                .unwrap(),
            Some(version + 1)
        );
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn push_with_a_commit_larger_than_the_forge_reads_is_refused() {
        let mut fixture = Fixture::new().await;
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        // The commit is made in a clone: an object the repository already holds never
        // reaches the push quarantine.
        let clone_path = fixture._directory.path().join("clone.git");
        let clone = git2::build::RepoBuilder::new()
            .bare(true)
            .clone(fixture.path.to_str().unwrap(), &clone_path)
            .unwrap();
        let parent = clone.find_commit(fixture.oid).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let message = "x".repeat(crate::backend::git::repository::MAX_COMMIT_OBJECT_BYTES + 1);
        let commit = clone
            .commit(
                None,
                &signature,
                &signature,
                &message,
                &parent.tree().unwrap(),
                &[&parent],
            )
            .unwrap();
        fixture.request = receive_request(
            &clone_path,
            "refs/heads/main",
            &fixture.oid.to_string(),
            &commit.to_string(),
        );
        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("ng refs/heads/main"), "{output}");
        assert_eq!(
            bare.find_reference("refs/heads/main").unwrap().target(),
            Some(fixture.oid)
        );
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn new_branch_receive_hook_filters_paths_against_the_root_tree() {
        let mut fixture = Fixture::new().await;
        fixture.ci = Arc::new(CiRuns::new(
            fixture.pool.clone(),
            Arc::new(crate::backend::ci::CiConfig {
                trusted_actor_ids: [fixture.actor_id].into(),
                ..Default::default()
            }),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &fixture.pool,
            )),
        ));
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let matching = bare
            .blob(b"name: Matching\non:\n  push:\n    paths: ['src/**']\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps: []\n")
            .unwrap();
        let nonmatching = bare
            .blob(b"name: Nonmatching\non:\n  push:\n    paths: ['docs/**']\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps: []\n")
            .unwrap();
        let oversized = bare
            .blob(&vec![b'#'; crate::backend::git::MAX_CI_WORKFLOW_BYTES + 1])
            .unwrap();
        let mut workflows = bare.treebuilder(None).unwrap();
        workflows
            .insert("matching.yml", matching, 0o100644)
            .unwrap();
        workflows
            .insert("nonmatching.yml", nonmatching, 0o100644)
            .unwrap();
        workflows
            .insert("oversized.yml", oversized, 0o100644)
            .unwrap();
        let workflows = workflows.write().unwrap();
        let mut github = bare.treebuilder(None).unwrap();
        github.insert("workflows", workflows, 0o040000).unwrap();
        let github = github.write().unwrap();
        let mut src = bare.treebuilder(None).unwrap();
        src.insert(
            "lib.rs",
            bare.blob(b"pub fn added() {}\n").unwrap(),
            0o100644,
        )
        .unwrap();
        let src = src.write().unwrap();
        let mut root = bare.treebuilder(None).unwrap();
        root.insert(".github", github, 0o040000).unwrap();
        root.insert("src", src, 0o040000).unwrap();
        let root = bare.find_tree(root.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let commit = bare
            .commit(None, &signature, &signature, "new branch", &root, &[])
            .unwrap();
        let commit_sha = commit.to_string();
        fixture.request = receive_request(
            &fixture.path,
            "refs/heads/path-filter",
            ZERO_SHA,
            &commit_sha,
        );

        let output = timeout(std::time::Duration::from_secs(15), fixture.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/path-filter"));
        let runs = db::ci::list_ci_runs_for_repo(&fixture.pool, fixture.repo_id, 10)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].workflow_path, ".github/workflows/matching.yml");
        assert_eq!(runs[0].head_sha.as_deref(), Some(commit_sha.as_str()));
        fixture.pool.close().await;
    }

    #[tokio::test]
    async fn accepted_updates_are_processed_after_a_later_push_changes_the_ref() {
        for change in ["advance", "delete", "recreate", "accepted-delete"] {
            let mut fixture = Fixture::new().await;
            let bare = git2::Repository::open_bare(&fixture.path).unwrap();
            bare.set_head("refs/heads/main").unwrap();
            let definition = "on: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo accepted\n";
            let blob = bare.blob(definition.as_bytes()).unwrap();
            let malformed = bare.blob(b"name: broken\non: [push\n").unwrap();
            let mut workflows = bare.treebuilder(None).unwrap();
            workflows.insert("ci.yml", blob, 0o100644).unwrap();
            workflows.insert("broken.yml", malformed, 0o100644).unwrap();
            let workflows = workflows.write().unwrap();
            let mut github = bare.treebuilder(None).unwrap();
            github.insert("workflows", workflows, 0o040000).unwrap();
            let github = github.write().unwrap();
            let mut root = bare.treebuilder(None).unwrap();
            root.insert(".github", github, 0o040000).unwrap();
            let root = bare.find_tree(root.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
            let parent = bare.find_commit(fixture.oid).unwrap();
            let first = bare
                .commit(None, &signature, &signature, "Push A", &root, &[&parent])
                .unwrap();
            let first_commit = bare.find_commit(first).unwrap();
            let later = bare
                .commit(
                    None,
                    &signature,
                    &signature,
                    "Push B",
                    &root,
                    &[&first_commit],
                )
                .unwrap();
            bare.reference("refs/heads/topic", fixture.oid, false, "fixture")
                .unwrap();
            let pull = db::pull_requests::records::create_pull_request(
                &fixture.pool,
                db::pull_requests::records::NewPullRequest {
                    repo_id: fixture.repo_id,
                    head_repo_id: None,
                    author_id: fixture.actor_id,
                    title: "Topic",
                    body: "",
                    head_branch: "topic",
                    head_sha: &fixture.oid.to_string(),
                    base_branch: "main",
                    base_sha: &fixture.oid.to_string(),
                    is_draft: false,
                },
            )
            .await
            .unwrap();
            fixture.ci = Arc::new(CiRuns::new(
                fixture.pool.clone(),
                Arc::new(crate::backend::ci::CiConfig {
                    trusted_actor_ids: [fixture.actor_id].into(),
                    ..Default::default()
                }),
                Arc::new(tokio::sync::Notify::new()),
                None,
                Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                    &fixture.pool,
                )),
            ));
            fn request(path: &Path, old: &str, new: &str) -> Vec<u8> {
                use std::io::Write;
                let command = format!("{old} {new} refs/heads/topic\0report-status\n");
                let mut request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
                if new != ZERO_SHA {
                    let mut pack = std::process::Command::new("git")
                        .arg("-C")
                        .arg(path)
                        .args(["pack-objects", "--stdout", "--revs"])
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        .unwrap();
                    let mut input = pack.stdin.take().unwrap();
                    writeln!(input, "{new}").unwrap();
                    if old != ZERO_SHA {
                        writeln!(input, "^{old}").unwrap();
                    }
                    drop(input);
                    let output = pack.wait_with_output().unwrap();
                    assert!(output.status.success());
                    request.extend_from_slice(&output.stdout);
                }
                request
            }
            let accepted = if change == "accepted-delete" {
                ZERO_SHA.to_owned()
            } else {
                first.to_string()
            };
            fixture.request = request(&fixture.path, &fixture.oid.to_string(), &accepted);
            let (entered, reached) = oneshot::channel();
            let (release, wait) = oneshot::channel();
            BARRIERS.lock().unwrap().insert(
                (fixture.path.clone(), true),
                Barrier {
                    entered,
                    release: wait,
                },
            );
            let mut task = fixture.receive();
            hook_reached(reached, &mut task).await;
            assert_eq!(
                bare.find_reference("refs/heads/topic")
                    .ok()
                    .and_then(|r| r.target()),
                (change != "accepted-delete").then_some(first)
            );
            // A has already been accepted by Git. B is another real receive-pack,
            // completed before A's post-hook can resume.
            if matches!(change, "delete" | "recreate") {
                fixture.request = request(&fixture.path, &first.to_string(), ZERO_SHA);
                let output = fixture.receive().await.unwrap().unwrap();
                assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/topic"));
            }
            if change != "delete" {
                let old = if matches!(change, "recreate" | "accepted-delete") {
                    ZERO_SHA.to_owned()
                } else {
                    first.to_string()
                };
                fixture.request = request(&fixture.path, &old, &later.to_string());
                let output = fixture.receive().await.unwrap().unwrap();
                assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/topic"));
            }
            release.send(()).unwrap();
            let output = timeout(std::time::Duration::from_secs(15), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&output).contains("ok refs/heads/topic"));
            let runs = db::ci::list_ci_runs_for_repo(&fixture.pool, fixture.repo_id, 10)
                .await
                .unwrap();
            assert_eq!(
                runs.iter()
                    .filter(|r| r.head_sha.as_deref() == Some(first.to_string().as_str()))
                    .count(),
                usize::from(change != "accepted-delete")
            );
            assert_eq!(
                runs.iter()
                    .filter(|r| r.head_sha.as_deref() == Some(later.to_string().as_str()))
                    .count(),
                usize::from(change != "delete")
            );
            assert_eq!(
                runs.len(),
                if matches!(change, "advance" | "recreate") {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                db::ci::has_ci_accepted_push(
                    &fixture.pool,
                    fixture.repo_id,
                    fixture.actor_id,
                    "refs/heads/topic",
                    &first.to_string()
                )
                .await
                .unwrap(),
                change != "accepted-delete"
            );
            let moved = db::pull_requests::records::get_pull_request_by_id(&fixture.pool, pull.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                moved.head_sha,
                if change == "delete" {
                    fixture.oid.to_string()
                } else {
                    later.to_string()
                }
            );
            let repository =
                db::repositories::records::get_repository_by_id(&fixture.pool, fixture.repo_id)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(repository.default_branch, "main");
            assert!(repository.size_bytes > 0);
            for _ in 0..runs.len() {
                let run =
                    db::ci::claim_ci_run(&fixture.pool, "worker", "local", &[fixture.actor_id])
                        .await
                        .unwrap()
                        .unwrap();
                assert_eq!(run.workflow_path, ".github/workflows/ci.yml");
                assert_eq!(
                    run.workflow_blobs[".github/workflows/ci.yml"],
                    blob.to_string()
                );
                assert!(run
                    .workflow_blobs
                    .get(".github/workflows/broken.yml")
                    .is_none());
                let store = repo_store(&repository, None);
                let checkout = fixture
                    ._directory
                    .path()
                    .join(format!("checkout-{}", run.id));
                store
                    .checkout(&checkout, run.head_sha.as_deref().unwrap())
                    .await
                    .unwrap();
                let cloned = git2::Repository::open(checkout).unwrap();
                assert_eq!(
                    cloned.head().unwrap().target().unwrap().to_string(),
                    run.head_sha.as_ref().unwrap().as_str()
                );
                db::ci::finish_ci_run(&fixture.pool, run.id, "worker", "success", None)
                    .await
                    .unwrap()
                    .unwrap();
            }
            fixture.pool.close().await;
        }
    }

    #[tokio::test]
    async fn post_receive_failure_is_reported_without_rolling_back() {
        let mut fixture = Fixture::new().await;
        fixture.delete_spare_branch();
        let (entered, reached) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        BARRIERS.lock().unwrap().insert(
            (fixture.path.clone(), true),
            Barrier {
                entered,
                release: wait,
            },
        );
        let mut task = fixture.receive();
        hook_reached(reached, &mut task).await;
        assert!(git2::Repository::open_bare(&fixture.path)
            .unwrap()
            .find_reference("refs/heads/spare")
            .is_err());
        assert!(
            db::users::ssh_keys::delete_ssh_key(&fixture.pool, fixture.key_id)
                .await
                .unwrap()
        );
        release.send(()).unwrap();
        let error = timeout(std::time::Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.is::<super::super::super::PostReceiveFailed>());
        assert!(git2::Repository::open_bare(&fixture.path)
            .unwrap()
            .find_reference("refs/heads/spare")
            .is_err());
        fixture.pool.close().await;
    }
}
