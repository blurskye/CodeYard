//! Large and multi-round Git transfers through real HTTP and built-in SSH servers.
use super::*;
use rand::{RngCore, SeedableRng};
use std::io::Write;

fn commits(path: &Path, count: usize) {
    let repo = git2::Repository::open(path).unwrap();
    let signature = git2::Signature::now("Tester", "streaming@example.invalid").unwrap();
    for number in 0..count {
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!(
                "{} commit {number}",
                path.file_name().unwrap().to_string_lossy()
            ),
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    }
}

fn peak_memory(server: &Server) -> u64 {
    std::fs::read_to_string(format!("/proc/{}/status", server.child.id()))
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .map(|value| value.split_whitespace().next().unwrap().parse().unwrap())
        })
        .unwrap()
}

pub async fn exercise(remote: bool) {
    bundle::install();
    let database = native::PostgresDatabase::new().await;
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let owner = db::users::accounts::create_user(&database.pool, "stream_owner", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    assert!(Command::new(tool("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap()
        .status
        .success());
    db::users::ssh_keys::add_ssh_key(
        &database.pool,
        owner.id,
        "streaming",
        &std::fs::read_to_string(key.with_extension("pub")).unwrap(),
    )
    .await
    .unwrap();
    let cookie_token = native::token();
    db::users::sessions::create_session(
        &database.pool,
        &cookie_token,
        owner.id,
        None,
        None,
        Utc::now() + ChronoDuration::hours(1),
    )
    .await
    .unwrap();
    let token = native::token();
    tokens::create_token(
        &database.pool,
        owner.id,
        &cookie_token,
        "streaming",
        &hex::encode(Sha256::digest(token.as_bytes())),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let cookie = format!("session_id={cookie_token}");
    let csrf = native::token();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(
            &database.pool,
            &cookie_token,
            Some(owner.id),
            &csrf,
            3600
        )
        .await
        .unwrap(),
        Some(csrf.clone())
    );
    let mut storage = if remote {
        Some(
            Server::start(
                &root.path().join("storage"),
                &database,
                "stream-storage",
                "storage",
                owner.id,
            )
            .await,
        )
    } else {
        None
    };
    let web = Server::start(
        &root.path().join("web"),
        &database,
        "stream-web",
        if remote { "web,ssh" } else { "web,ssh,storage" },
        owner.id,
    )
    .await;
    let client = Client::new();
    let work = root.path().join("clients");
    std::fs::create_dir(&work).unwrap();
    success(git(&work, &["init", "-q", "-b", "main", "source"], None, &key).await);
    std::fs::write(work.join("source/README"), "streaming\n").unwrap();
    success(git(&work, &["-C", "source", "add", "."], None, &key).await);
    success(
        git(
            &work,
            &["-C", "source", "commit", "-qm", "Initial"],
            None,
            &key,
        )
        .await,
    );
    commits(&work.join("source"), 125);
    let random_path = work.join("random.bin");
    let path = random_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut random = rand::rngs::StdRng::seed_from_u64(42);
        let mut bytes = vec![0; 1024 * 1024];
        let mut file = std::fs::File::create(path).unwrap();
        for _ in 0..150 {
            random.fill_bytes(&mut bytes);
            file.write_all(&bytes).unwrap();
        }
    })
    .await
    .unwrap();
    let mut last_url = String::new();
    for transport in ["ssh", "http"] {
        let name = format!("stream-{transport}");
        let created = api(
            &client,
            &web,
            (&cookie, &csrf),
            reqwest::Method::POST,
            "/api/repos",
            json!({"name":name,"is_private":true}),
            StatusCode::CREATED,
        )
        .await;
        let repo_id = created["id"].as_i64().unwrap();
        let url = if transport == "ssh" {
            format!(
                "ssh://git@127.0.0.1:{}/stream_owner/{name}.git",
                web.ssh_port
            )
        } else {
            format!("{}/stream_owner/{name}.git", web.base)
        };
        let auth = (transport == "http").then_some(token.as_str());
        success(
            git(
                &work,
                &["-C", "source", "push", "-q", &url, "main"],
                auth,
                &key,
            )
            .await,
        );
        let a = format!("{transport}-a");
        let b = format!("{transport}-b");
        for destination in [&a, &b] {
            success(git(&work, &["clone", "-q", &url, destination], auth, &key).await);
        }
        commits(&work.join(&b), 3);
        success(
            git(
                &work,
                &["-C", &b, "push", "-q", "origin", "main"],
                auth,
                &key,
            )
            .await,
        );
        success(git(&work, &["-C", &a, "fetch", "-q", "origin"], auth, &key).await);
        let expected = success(git(&work, &["-C", &b, "rev-parse", "HEAD"], None, &key).await);
        assert_eq!(
            success(git(&work, &["-C", &a, "rev-parse", "origin/main"], None, &key).await),
            expected
        );
        // The first negotiation batch consists entirely of non-common commits.
        commits(&work.join(&a), 40);
        commits(&work.join(&b), 3);
        success(
            git(
                &work,
                &["-C", &b, "push", "-q", "origin", "main"],
                auth,
                &key,
            )
            .await,
        );
        success(git(&work, &["-C", &a, "fetch", "-q", "origin"], auth, &key).await);
        assert_eq!(
            success(git(&work, &["-C", &a, "rev-parse", "origin/main"], None, &key).await),
            success(git(&work, &["-C", &b, "rev-parse", "HEAD"], None, &key).await)
        );
        success(
            git(
                &work,
                &["-C", &b, "push", "-q", "origin", "main:delete-me"],
                auth,
                &key,
            )
            .await,
        );
        success(
            git(
                &work,
                &["-C", &b, "push", "-q", "origin", "--delete", "delete-me"],
                auth,
                &key,
            )
            .await,
        );
        assert!(success(
            git(
                &work,
                &["ls-remote", &url, "refs/heads/delete-me"],
                auth,
                &key
            )
            .await
        )
        .is_empty());
        std::fs::copy(&random_path, work.join(&b).join("random.bin")).unwrap();
        success(git(&work, &["-C", &b, "add", "random.bin"], None, &key).await);
        success(
            git(
                &work,
                &["-C", &b, "commit", "-qm", "Large pack"],
                None,
                &key,
            )
            .await,
        );
        success(
            git(
                &work,
                &[
                    "-c",
                    "http.postBuffer=1024",
                    "-C",
                    &b,
                    "push",
                    "-q",
                    "origin",
                    "main",
                ],
                auth,
                &key,
            )
            .await,
        );
        let large = format!("{transport}-large");
        success(git(&work, &["clone", "-q", &url, &large], auth, &key).await);
        assert_eq!(
            success(
                git(
                    &work,
                    &["-C", &large, "rev-parse", "HEAD^{tree}"],
                    None,
                    &key
                )
                .await
            ),
            success(git(&work, &["-C", &b, "rev-parse", "HEAD^{tree}"], None, &key).await)
        );
        assert!(
            std::fs::read_dir(work.join(&large).join(".git/objects/pack"))
                .unwrap()
                .filter_map(Result::ok)
                .any(
                    |entry| entry.path().extension().is_some_and(|ext| ext == "pack")
                        && entry.metadata().unwrap().len() > 128 * 1024 * 1024
                )
        );
        db::repositories::protection::upsert_branch_protection_rule(
            &database.pool,
            BranchProtectionRuleUpsert {
                actor_id: owner.id,
                repo_id,
                pattern: "main",
                require_pull_request: true,
                required_approving_reviews: 0,
                dismiss_stale_reviews: false,
                require_status_checks: false,
                required_status_checks: vec![],
                require_up_to_date_branch: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        commits(&work.join(&b), 1);
        let rejected = git(&work, &["-C", &b, "push", "origin", "main"], auth, &key).await;
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("pre-receive hook declined"));
        last_url = url;
    }
    assert!(
        peak_memory(&web) < 192 * 1024,
        "web process used {} KiB",
        peak_memory(&web)
    );
    if let Some(storage) = storage.as_mut() {
        assert!(
            peak_memory(storage) < 192 * 1024,
            "storage process used {} KiB",
            peak_memory(storage)
        );
        let root = work.clone();
        let key = key.clone();
        let interrupted = tokio::spawn(async move {
            git(
                &root,
                &["clone", "-q", &last_url, "interrupted"],
                Some(&token),
                &key,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let pack = work.join("interrupted/.git/objects/pack");
                if std::fs::read_dir(pack).is_ok_and(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .any(|entry| entry.file_name().to_string_lossy().starts_with("tmp_pack_"))
                }) {
                    break;
                }
                assert!(
                    !interrupted.is_finished(),
                    "clone finished before storage could be interrupted"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("clone did not start receiving its pack");
        storage.child.kill().unwrap();
        storage.child.wait().unwrap();
        // The web node notices a silent peer after the 15 s QUIC idle timeout.
        let result = tokio::time::timeout(Duration::from_secs(60), interrupted)
            .await
            .expect("storage outage hung Git")
            .unwrap();
        assert!(
            !result.status.success(),
            "interrupted clone reported success"
        );
    }
}
