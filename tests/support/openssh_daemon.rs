//! Real loopback OpenSSH authentication/forced-command/Git acceptance.
//! The sole OS fixture substitution is the login shell of the current user (often
//! nologin), via a test-only preload in the disposable sshd (never in the app or host sshd).
//! No account, host key, system config, sudo policy or existing service is changed.
use super::*;

async fn output(mut command: Command) -> Output {
    timeout(
        Duration::from_secs(45),
        command.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .expect("fixture command timeout")
    .expect("fixture command start")
}
/// `name` from PATH, then the directories where distributions keep sshd, then the NixOS system
/// profile as a last resort.
pub(super) fn tool(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain(["/usr/sbin", "/usr/local/sbin", "/run/current-system/sw/bin"].map(PathBuf::from))
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("required fixture tool: {name}"))
}
fn success(result: Output) -> Output {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
fn ssh(config: &Path) -> Command {
    let mut command = Command::new(tool("ssh"));
    command.args(["-F"]).arg(config);
    command
}
async fn git(broker: &Broker, config: &Path, cwd: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::new(&broker.gateway.git);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", broker.directory.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env(
            "GIT_SSH_COMMAND",
            format!("{} -F {}", tool("ssh").display(), config.display()),
        )
        .env("GIT_SSH_VARIANT", "ssh")
        .current_dir(cwd)
        .args(arguments);
    output(command).await
}

#[tokio::test]
#[ignore = "needs sshd, cc and an unprivileged user"]
async fn real_openssh_signed_key_forced_commands_streamed_git_and_revocation() {
    real_fixture(None).await;
}

#[tokio::test]
#[ignore = "requires GITSERV_TEST_ACT_BINARY set to the pinned act; trusted fixture only"]
async fn real_openssh_push_ci_outlives_helper() {
    use sha2::Digest;
    let act = PathBuf::from(
        std::env::var("GITSERV_TEST_ACT_BINARY").expect("verified act fixture required"),
    );
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(std::fs::read(&act).unwrap())),
        "6be37b104430efc210d5130495bedcff2dc7cd6780a38d88f3d205e7f1185cc1"
    );
    real_fixture(Some(act)).await;
}

async fn real_fixture(act: Option<PathBuf>) {
    assert_ne!(
        private_files::effective_uid().unwrap(),
        0,
        "unprivileged fixture user required"
    );
    let mut id = Command::new(tool("id"));
    id.arg("-un");
    let user = String::from_utf8(success(output(id).await).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let database = database().await;
    let actor = db::users::accounts::create_user(&database.pool, "alice", None)
        .await
        .unwrap();
    let run_ci = act.is_some();
    let act_home = tempfile::Builder::new()
        .prefix("openssh-act-")
        .tempdir()
        .unwrap();
    let ci_config = act.map(|act| {
        let binary = validation::shell_safe_path(&act).unwrap();
        let home = validation::shell_safe_path(act_home.path()).unwrap();
        let wrapper = act_home.path().join("act-owned");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexport HOME='{home}'\nexport XDG_CONFIG_HOME=\"$HOME/config\" XDG_CACHE_HOME=\"$HOME/cache\"\nunset DOCKER_HOST DOCKER_CONTEXT SSH_AUTH_SOCK GITHUB_TOKEN GH_TOKEN\nexec '{binary}' --no-cache-server --action-cache-path \"$HOME/actions\" \"$@\"\n")).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::backend::ci::CiConfig {
            mode: crate::backend::ci::CiExecutionMode::Local,
            trusted_actor_ids: [actor.id].into(),
            act_binary: wrapper.to_str().unwrap().to_owned(),
            act_platform_map: [("gitserv-owned-fixture".to_owned(), "-self-hosted".to_owned())].into(),
            ..Default::default()
        }
    });
    let broker = Broker::new_with_ci(database.pool.clone(), user.clone(), ci_config).await;
    // OpenSSH StrictModes does not allow /tmp ancestors, even sticky ones.
    // A newly owned private directory under the user's home needs no exception.
    let files = tempfile::Builder::new()
        .prefix("gitserv-openssh-daemon-")
        .tempdir_in(std::env::var_os("HOME").expect("owned HOME required"))
        .unwrap();
    let root = files.path();
    let shim = root.join("shell-fixture.so");
    let source = root.join("shell-fixture.c");
    std::fs::write(&source, include_str!("openssh_test_shell.c")).unwrap();
    let mut compiler = Command::new("cc");
    compiler
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .args(["-o"])
        .arg(&shim)
        .arg("-ldl");
    success(output(compiler).await);
    for name in ["host", "client", "unknown"] {
        let mut keygen = Command::new(tool("ssh-keygen"));
        keygen
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(root.join(name));
        success(output(keygen).await);
    }
    let public = std::fs::read_to_string(root.join("client.pub")).unwrap();
    let key =
        db::users::ssh_keys::add_ssh_key(&database.pool, actor.id, "real OpenSSH fixture", &public)
            .await
            .unwrap();
    let mut fingerprint = Command::new(tool("ssh-keygen"));
    fingerprint.args(["-lf"]).arg(root.join("client.pub"));
    let fingerprint = String::from_utf8(success(output(fingerprint).await).stdout).unwrap();
    assert_eq!(
        fingerprint.split_whitespace().nth(1),
        Some(key.fingerprint.as_str())
    );
    let authorized = success(broker.keys(&public, &user).await).stdout;
    assert!(authorized.starts_with(b"restrict,command=\""));
    std::fs::write(root.join("authorized_keys"), &authorized).unwrap();
    std::fs::set_permissions(
        root.join("authorized_keys"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let host_public = std::fs::read_to_string(root.join("host.pub")).unwrap();
    std::fs::write(
        root.join("known_hosts"),
        format!("openssh-owned-fixture {host_public}"),
    )
    .unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let server_config = root.join("sshd_config");
    std::fs::write(&server_config, format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {0}/host\nPidFile {0}/sshd.pid\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAuthenticationMethods publickey\nPermitRootLogin no\nAllowUsers {user}\nAuthorizedKeysFile {0}/authorized_keys\nAuthorizedKeysCommand none\nStrictModes yes\nDisableForwarding yes\nPermitTTY no\nPermitUserRC no\nPermitUserEnvironment no\nAcceptEnv GIT_PROTOCOL\nPerSourcePenalties no\nUseDNS no\nLogLevel VERBOSE\n", root.display())).unwrap();
    let config = root.join("ssh_config");
    std::fs::write(&config, format!(
        "Host owned-fixture\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {0}/client\n  IdentityAgent none\n  IdentitiesOnly yes\n  BatchMode yes\n  StrictHostKeyChecking yes\n  HostKeyAlias openssh-owned-fixture\n  UserKnownHostsFile {0}/known_hosts\n  GlobalKnownHostsFile /dev/null\n  ForwardAgent no\n  ConnectTimeout 5\n  SendEnv GIT_PROTOCOL\n", root.display())).unwrap();
    let mut check = Command::new(tool("sshd"));
    check.args(["-t", "-f"]).arg(&server_config);
    success(output(check).await);
    drop(reservation);
    let log = root.join("sshd.log");
    let mut command = Command::new(tool("sshd"));
    command
        .env(
            "PATH",
            "/usr/local/bin:/usr/bin:/bin:/run/current-system/sw/bin",
        )
        .env("LD_PRELOAD", &shim)
        .args(["-D", "-E"])
        .arg(&log)
        .arg("-f")
        .arg(&server_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut owned = command.spawn().unwrap();
    let (daemon_stop, mut stopped) = tokio::sync::watch::channel(false);
    let daemon = tokio::spawn(async move {
        tokio::select! {
            status = owned.wait() => Ok::<_, anyhow::Error>(status?),
            _ = stopped.changed() => {
                owned.kill().await?;
                anyhow::bail!("sshd stopped")
            }
            _ = sleep(Duration::from_secs(180)) => anyhow::bail!("sshd fixture timed out"),
        }
    });
    let mut ready = false;
    for _ in 0..50 {
        if daemon.is_finished() {
            let status = daemon.await.unwrap();
            panic!(
                "owned sshd exited ({status:?}): {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "owned sshd did not listen");
    // Repository directories are named by a UUID, whatever the repository is called.
    let bare_path = broker
        .gateway
        .repos_root
        .join(format!("{}.git", uuid::Uuid::new_v4()));
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    let bare = git2::Repository::init_opts(&bare_path, &options).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        actor.id,
        "private",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let source_path = broker.directory.path().join("source");
    options.bare(false);
    let local = git2::Repository::init_opts(&source_path, &options).unwrap();
    let mut binary = vec![0; 4 * 1024 * 1024];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut binary);
    std::fs::write(source_path.join("binary.dat"), &binary).unwrap();
    let mut index = local.index().unwrap();
    index.add_path(Path::new("binary.dat")).unwrap();
    let release = broker.directory.path().join("release-ci-after-ssh-exit");
    if run_ci {
        use sha2::Digest;
        let digest = format!("{:x}", sha2::Sha256::digest(&binary));
        std::fs::create_dir_all(source_path.join(".github/workflows")).unwrap();
        std::fs::write(source_path.join(".github/workflows/ci.yml"), format!(
            "name: Owned SSH push\non:\n  push:\n    branches: [main]\njobs:\n  proof:\n    runs-on: gitserv-owned-fixture\n    steps:\n      - shell: bash\n        run: |\n          set -eu\n          n=0\n          while [ ! -f '{0}' ] && [ \"$n\" -lt 300 ]; do n=$((n + 1)); sleep 0.1; done\n          test -f '{0}'\n          git clone -q --no-local -- '{1}' verified\n          git -C verified checkout --detach \"$GITHUB_SHA\"\n          test \"$(git -C verified rev-parse HEAD)\" = \"$GITHUB_SHA\"\n          printf '%s  verified/binary.dat\\n' '{digest}' | sha256sum -c -\n          printf 'OPENSSH_PUSH_CI_PASSED HEAD=%s\\n' \"$GITHUB_SHA\"\n", release.display(), bare_path.display())).unwrap();
        index
            .add_path(Path::new(".github/workflows/ci.yml"))
            .unwrap();
        std::fs::write(source_path.join(".github/workflows/tags.yml"), format!(
            "name: Owned SSH tag\non:\n  push:\n    tags: ['release-*']\njobs:\n  check:\n    runs-on: gitserv-owned-fixture\n    steps:\n      - shell: bash\n        run: |\n          set -eu\n          case \"$GITHUB_REF\" in refs/tags/release-*) ;; *) exit 1 ;; esac\n          git clone -q --no-local -- '{}' verified\n          git -C verified checkout -q --detach \"$GITHUB_SHA\"\n          test \"$(git -C verified rev-parse HEAD)\" = \"$GITHUB_SHA\"\n          test \"$(git -C verified cat-file -t \"$GITHUB_SHA\")\" = commit\n          test -f verified/.github/workflows/tags.yml\n          printf 'OPENSSH_TAG_CI_PASSED HEAD=%s REF=%s\\n' \"$GITHUB_SHA\" \"$GITHUB_REF\"\n", bare_path.display())).unwrap();
        index
            .add_path(Path::new(".github/workflows/tags.yml"))
            .unwrap();
    }
    index.write().unwrap();
    drop(index);
    let head = commit(&local, "real SSH transport\n");
    let pushed = git(
        &broker,
        &config,
        &source_path,
        &[
            "push",
            "ssh://owned-fixture/alice/private.git",
            "HEAD:refs/heads/main",
        ],
    )
    .await;
    assert!(
        pushed.status.success(),
        "real ssh push: {}\nsshd: {}",
        String::from_utf8_lossy(&pushed.stderr),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(
        bare.find_reference("refs/heads/main").unwrap().target(),
        Some(head)
    );
    if run_ci {
        let runs = db::ci::list_ci_runs_for_repo(&database.pool, repo.id, 10)
            .await
            .unwrap();
        assert_eq!(
            runs.len(),
            1,
            "real push must enqueue exactly one persistent run"
        );
        let run = &runs[0];
        assert_eq!(run.head_sha.as_deref(), Some(head.to_string().as_str()));
        assert_eq!(run.event, "push");
        assert_eq!(run.triggered_by_id, Some(actor.id));
        assert!(
            matches!(run.status.as_str(), "queued" | "running"),
            "CI before release: {run:?}"
        );
        // The real SSH client/helper has exited; only now may the workflow finish.
        std::fs::write(&release, b"SSH helper exited\n").unwrap();
        let finished = timeout(Duration::from_secs(90), async {
            loop {
                let current = db::ci::get_ci_run_by_run_id(&database.pool, run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                if !matches!(current.status.as_str(), "queued" | "running") {
                    break current;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("persistent real act workflow timeout");
        let logs = db::ci::list_ci_logs(&database.pool, run.id, 1000)
            .await
            .unwrap();
        assert_eq!(finished.status, "success", "{finished:?}\n{logs:?}");
        let marker = format!("OPENSSH_PUSH_CI_PASSED HEAD={head}");
        assert!(
            logs.iter().any(|line| line.line.contains(&marker)),
            "missing real workflow marker: {logs:?}"
        );
        println!(
            "real OpenSSH persistent act run {} succeeded at {head} after helper exit",
            run.run_id
        );
        let target = local.find_object(head, None).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let tag_oid = local
            .tag("release-annotated", &target, &signature, "release", false)
            .unwrap();
        assert_ne!(tag_oid, head);
        local
            .reference("refs/tags/release-lightweight", head, false, "release")
            .unwrap();
        success(
            git(
                &broker,
                &config,
                &source_path,
                &[
                    "push",
                    "ssh://owned-fixture/alice/private.git",
                    "refs/tags/release-annotated",
                    "refs/tags/release-lightweight",
                ],
            )
            .await,
        );
        let tagged = db::ci::list_ci_runs_for_repo(&database.pool, repo.id, 10)
            .await
            .unwrap();
        assert_eq!(tagged.len(), 3, "one branch run and one per accepted tag");
        let expected_blob = local
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_path(Path::new(".github/workflows/tags.yml"))
            .unwrap()
            .id()
            .to_string();
        for reference in [
            "refs/tags/release-annotated",
            "refs/tags/release-lightweight",
        ] {
            let runs: Vec<_> = tagged.iter().filter(|r| r.branch == reference).collect();
            assert_eq!(runs.len(), 1);
            let run = runs[0];
            assert_eq!(run.head_sha.as_deref(), Some(head.to_string().as_str()));
            assert_eq!(run.workflow_path, ".github/workflows/tags.yml");
            assert_eq!(
                run.workflow_blobs[".github/workflows/tags.yml"],
                expected_blob
            );
            assert!(db::ci::has_ci_accepted_push(
                &database.pool,
                repo.id,
                actor.id,
                reference,
                &head.to_string()
            )
            .await
            .unwrap());
            assert!(!db::ci::has_ci_accepted_push(
                &database.pool,
                repo.id,
                actor.id,
                reference,
                &tag_oid.to_string()
            )
            .await
            .unwrap());
            let finished = timeout(Duration::from_secs(90), async {
                loop {
                    let current = db::ci::get_ci_run_by_run_id(&database.pool, run.run_id)
                        .await
                        .unwrap()
                        .unwrap();
                    if !matches!(current.status.as_str(), "queued" | "running") {
                        break current;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("tag workflow timeout");
            let logs = db::ci::list_ci_logs(&database.pool, run.id, 1000)
                .await
                .unwrap();
            assert_eq!(finished.status, "success", "{finished:?}\n{logs:?}");
            let marker = format!("OPENSSH_TAG_CI_PASSED HEAD={head} REF={reference}");
            assert!(
                logs.iter().any(|line| line.line.contains(&marker)),
                "missing tag workflow marker: {logs:?}"
            );
        }
        // A tag of a blob is a valid Git ref but cannot authorize a commit run.
        let blob_target = local
            .find_object(git2::Oid::from_str(&expected_blob).unwrap(), None)
            .unwrap();
        let noncommit = local
            .tag(
                "release-blob",
                &blob_target,
                &signature,
                "not a commit",
                false,
            )
            .unwrap();
        let _post_receive_error = git(
            &broker,
            &config,
            &source_path,
            &[
                "push",
                "ssh://owned-fixture/alice/private.git",
                "refs/tags/release-blob",
            ],
        )
        .await;
        assert_eq!(
            bare.find_reference("refs/tags/release-blob")
                .unwrap()
                .target(),
            Some(noncommit)
        );
        assert_eq!(
            db::ci::list_ci_runs_for_repo(&database.pool, repo.id, 10)
                .await
                .unwrap()
                .len(),
            3
        );
        assert!(!db::ci::has_ci_accepted_push(
            &database.pool,
            repo.id,
            actor.id,
            "refs/tags/release-blob",
            &noncommit.to_string()
        )
        .await
        .unwrap());
    }
    success(
        git(
            &broker,
            &config,
            broker.directory.path(),
            &[
                "clone",
                "ssh://owned-fixture/alice/private.git",
                "network-clone",
            ],
        )
        .await,
    );
    assert_eq!(
        git2::Repository::open(broker.directory.path().join("network-clone"))
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(head)
    );
    assert!(
        std::fs::read(broker.directory.path().join("network-clone/binary.dat")).unwrap() == binary,
        "4MiB incompressible binary changed across real SSH push/clone"
    );
    for command in [
        "echo unrestricted-shell",
        "git-upload-pack '../../../etc/passwd'",
        "git-upload-pack 'alice/private.git'; echo injected",
    ] {
        let mut request = ssh(&config);
        request.args(["-T", "owned-fixture", command]);
        let denied = output(request).await;
        assert!(!denied.status.success());
        assert!(
            denied.stdout.is_empty(),
            "shell/injection output escaped restriction"
        );
    }
    let mut shell = ssh(&config);
    shell.args(["-T", "owned-fixture"]);
    let shell = output(shell).await;
    assert!(!shell.status.success());
    assert!(shell.stdout.is_empty());
    let mut tty = ssh(&config);
    tty.args(["-tt", "owned-fixture", "echo shell"]);
    let tty = output(tty).await;
    assert!(!tty.status.success());
    assert!(String::from_utf8_lossy(&tty.stderr).contains("PTY allocation request failed"));
    let mut forward = ssh(&config);
    forward.args([
        "-N",
        "-o",
        "ExitOnForwardFailure=yes",
        "-R",
        "127.0.0.1:0:127.0.0.1:9",
        "owned-fixture",
    ]);
    let forward = output(forward).await;
    assert!(!forward.status.success());
    assert!(String::from_utf8_lossy(&forward.stderr).contains("remote port forwarding failed"));
    // Only this unknown identity is tried; do not fall back to the valid config identity.
    let unknown_config = root.join("unknown_config");
    std::fs::write(
        &unknown_config,
        std::fs::read_to_string(&config)
            .unwrap()
            .replace("/client\n", "/unknown\n"),
    )
    .unwrap();
    let denied = git(
        &broker,
        &unknown_config,
        &source_path,
        &["ls-remote", "ssh://owned-fixture/alice/private.git"],
    )
    .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("Permission denied"));
    assert!(!broker.keys(&public, &user).await.stdout.is_empty());
    success(
        git(
            &broker,
            &config,
            &source_path,
            &["ls-remote", "ssh://owned-fixture/alice/private.git"],
        )
        .await,
    );
    for column in ["is_banned", "must_change_password"] {
        sqlx::query(&format!("UPDATE users SET {column} = TRUE WHERE id = $1"))
            .bind(actor.id)
            .execute(&database.native)
            .await
            .unwrap();
        assert!(broker.keys(&public, &user).await.stdout.is_empty());
        assert!(!git(
            &broker,
            &config,
            &source_path,
            &["ls-remote", "ssh://owned-fixture/alice/private.git"]
        )
        .await
        .status
        .success());
        sqlx::query(&format!("UPDATE users SET {column} = FALSE WHERE id = $1"))
            .bind(actor.id)
            .execute(&database.native)
            .await
            .unwrap();
    }
    db::users::ssh_keys::delete_ssh_key(&database.pool, key.id)
        .await
        .unwrap();
    assert!(broker.keys(&public, &user).await.stdout.is_empty());
    // sshd keeps the prior authorized line. Fresh broker authorization still denies it.
    assert!(!git(
        &broker,
        &config,
        &source_path,
        &["ls-remote", "ssh://owned-fixture/alice/private.git"]
    )
    .await
    .status
    .success());
    daemon_stop.send(true).unwrap();
    let stopped = timeout(Duration::from_secs(10), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let _ = stopped;
    broker.stop().await;
}
