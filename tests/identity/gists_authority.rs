//! Live gist authority, publication recovery, and size boundaries on both engines.
use super::github::request;
use crate::support::database::Database;
use crate::support::http_client::Client;
use codeyard::backend::db;
use codeyard::backend::identity::tokens;
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;
use codeyard_app::gists;
use codeyard_app::gists::CreateGist;
use codeyard_app::repositories::access;
use codeyard_app::repositories::lifecycle;
use codeyard_app::repositories::store::gists::GistCredential;
use codeyard_app::repositories::store::repo_store;
use codeyard_app::storage::ObjectStore;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_git::gist_types::GistEdit;
use codeyard_git::gist_types::GistFileChange;
use codeyard_git::gist_types::MAX_GIST_BYTES;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn detached(path: &Path, base: &str) -> String {
    let repo = git2::Repository::open_bare(path).unwrap();
    let parent = repo.find_commit(base.parse().unwrap()).unwrap();
    let tree = parent.tree().unwrap();
    let mut builder = repo.treebuilder(Some(&tree)).unwrap();
    builder
        .insert(
            "recovered.txt",
            repo.blob(b"recovered\n").unwrap(),
            0o100644,
        )
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("alice", "alice@users.noreply").unwrap();
    repo.commit(
        None,
        &signature,
        &signature,
        "Recovered publication",
        &tree,
        &[&parent],
    )
    .unwrap()
    .to_string()
}
fn publish(path: &Path, sha: &str, old: &str) {
    git2::Repository::open_bare(path)
        .unwrap()
        .reference_matching(
            "refs/heads/main",
            sha.parse().unwrap(),
            true,
            old.parse().unwrap(),
            "publication fixture",
        )
        .unwrap();
}
fn edit(content: &str) -> GistEdit {
    GistEdit {
        files: BTreeMap::from([(
            "a.txt".into(),
            Some(GistFileChange {
                content: Some(content.into()),
                filename: None,
            }),
        )]),
        ..Default::default()
    }
}
async fn authority(pg: bool) {
    let _fixture = super::github_gists::fixture_slot().await;
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[bob])
        .await;
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        f.pool().clone(),
        root.path().into(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let router = create_router((*state).clone());
    let a = Client::login(&router, f.pool(), alice).await;
    let b = Client::login(&router, f.pool(), bob).await;
    let ca = GistCredential::Session(a.session.clone());
    let cb = GistCredential::Session(b.session.clone());
    let gist = gists::create(
        state.clone(),
        alice,
        ca.clone(),
        CreateGist {
            description: "Original".into(),
            is_public: false,
            files: BTreeMap::from([("a.txt".into(), "original\n".into())]),
        },
    )
    .await
    .unwrap();
    let repo = &gist.repository;
    let id = &gist.gist.id;
    let original = &gist.snapshot.sha;
    let path = format!("/api/v3/gists/{id}");
    let alice_user = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), alice)
        .await
        .unwrap()
        .unwrap();
    let admin = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), bob)
        .await
        .unwrap()
        .unwrap();
    assert!(!access::can_read(f.pool(), None, repo).await.unwrap());
    assert!(!access::can_admin(f.pool(), &admin, repo).await.unwrap());
    assert!(!access::user_access(f.pool(), &alice_user, repo)
        .await
        .unwrap()
        .can_read());
    assert!(access::user_git_access(f.pool(), &alice_user, repo)
        .await
        .unwrap()
        .can_write());
    assert!(!access::user_git_access(f.pool(), &admin, repo)
        .await
        .unwrap()
        .can_write());
    assert!(
        gists::update(state.clone(), bob, cb.clone(), id.clone(), edit("Not mine"))
            .await
            .is_err()
    );
    assert!(codeyard_db_identity::interactions::get_restriction(
        f.pool(),
        alice,
        codeyard_db_identity::interactions::InteractionScope::Repository(repo.id)
    )
    .await
    .is_err());
    assert!(codeyard_db_repos::pull_caps::get_cap(
        f.pool(),
        alice,
        codeyard_db_repos::pull_caps::PullCapScope::Repository(repo.id)
    )
    .await
    .is_err());
    let sha = detached(Path::new(&repo.disk_path), original);
    for published in [false, true] {
        codeyard_db_repos::gists::admit(
            f.pool(),
            alice,
            &AccountCredential::Session(&a.session),
            &codeyard_db_repos::gists::Admission {
                repo_id: repo.id,
                disk_path: &repo.disk_path,
                storage_node_id: Some(None),
                base: Some(original),
                sha: &sha,
                description: Some("Recovered description"),
            },
        )
        .await
        .unwrap();
        if published {
            assert!(codeyard_db_cluster::cluster::bump_write_version(
                f.pool(),
                repo.id,
                Some(None)
            )
            .await
            .unwrap());
            publish(Path::new(&repo.disk_path), &sha, original);
        }
        let loaded = gists::load(&state, id, None).await.unwrap();
        assert_eq!(
            &loaded.snapshot.sha,
            if published { &sha } else { original }
        );
        assert_eq!(
            loaded.gist.description,
            if published {
                "Recovered description"
            } else {
                "Original"
            }
        );
        assert!(codeyard_db_repos::gists::pending(f.pool(), repo.id)
            .await
            .unwrap()
            .is_none());
    }
    let before = gists::load(&state, id, None).await.unwrap().snapshot.sha;
    let invalid = GistCredential::Session("0".repeat(64));
    assert!(gists::update(
        state.clone(),
        alice,
        invalid,
        id.clone(),
        edit("Revoked session")
    )
    .await
    .is_err());
    let token = tokens::create(
        f.pool(),
        alice,
        Some(&a.session),
        TokenInput {
            name: "revoke",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    tokens::delete(f.pool(), alice, Some(&a.session), token.token.id)
        .await
        .unwrap();
    assert!(gists::update(
        state.clone(),
        alice,
        GistCredential::Token(token.token.id, String::new()),
        id.clone(),
        edit("Revoked token")
    )
    .await
    .is_err());
    assert_eq!(
        gists::load(&state, id, None).await.unwrap().snapshot.sha,
        before
    );
    // As on GitHub, a file object with neither content nor filename deletes the file.
    let deleted = gists::update(
        state.clone(),
        alice,
        ca.clone(),
        id.clone(),
        GistEdit {
            files: BTreeMap::from([("a.txt".into(), Some(GistFileChange::default()))]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let paths: Vec<_> = deleted
        .snapshot
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["recovered.txt"]);
    let comment = codeyard_db_repos::gists::create_comment(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        "Before blocking",
    )
    .await
    .unwrap();
    codeyard_db_repos::gists::star(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        true,
    )
    .await
    .unwrap();
    codeyard_db_identity::users::blocks::change_block(
        f.pool(),
        alice,
        &AccountCredential::Session(&a.session),
        bob,
        true,
    )
    .await
    .unwrap();
    assert!(!codeyard_db_repos::gists::starred(f.pool(), bob, repo.id)
        .await
        .unwrap());
    assert!(codeyard_db_repos::gists::star(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        true
    )
    .await
    .is_err());
    assert!(codeyard_db_repos::gists::create_comment(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        "Blocked"
    )
    .await
    .is_err());
    assert!(codeyard_db_repos::gists::change_comment(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        comment.id,
        Some("Blocked edit")
    )
    .await
    .is_err());
    assert!(gists::fork(state.clone(), bob, cb.clone(), id.clone())
        .await
        .is_err());
    request(&router, "gists/get", "GET", &path, None, Value::Null, 200).await;
    codeyard_db_repos::gists::change_comment(
        f.pool(),
        bob,
        &AccountCredential::Session(&b.session),
        repo.id,
        comment.id,
        None,
    )
    .await
    .unwrap();
    codeyard_db_identity::users::blocks::change_block(
        f.pool(),
        alice,
        &AccountCredential::Session(&a.session),
        bob,
        false,
    )
    .await
    .unwrap();
    assert!(!codeyard_db_repos::gists::starred(f.pool(), bob, repo.id)
        .await
        .unwrap());
    let count = f.count().await;
    codeyard_db_identity::users::sessions::delete_session(f.pool(), &b.session)
        .await
        .unwrap();
    assert!(gists::fork(state.clone(), bob, cb, id.clone())
        .await
        .is_err());
    assert_eq!(f.count().await, count);
    let large = gists::create(
        state.clone(),
        alice,
        ca.clone(),
        CreateGist {
            description: "Boundary".into(),
            is_public: false,
            files: BTreeMap::from([("big.txt".into(), "x".repeat(MAX_GIST_BYTES))]),
        },
    )
    .await
    .unwrap();
    let (_, body) = request(
        &router,
        "gists/get",
        "GET",
        &format!("/api/v3/gists/{}", large.gist.id),
        None,
        Value::Null,
        200,
    )
    .await;
    assert_eq!(body["files"]["big.txt"]["size"], MAX_GIST_BYTES);
    assert_eq!(body["files"]["big.txt"]["truncated"], true);
    assert_eq!(body["truncated"], false);
    assert_eq!(
        body["files"]["big.txt"]["content"].as_str().unwrap().len(),
        1024 * 1024
    );
    assert!(gists::update(
        state.clone(),
        alice,
        ca.clone(),
        large.gist.id.clone(),
        GistEdit {
            files: BTreeMap::from([(
                "extra.txt".into(),
                Some(GistFileChange {
                    content: Some("x".into()),
                    filename: None
                })
            )]),
            ..Default::default()
        }
    )
    .await
    .is_err());
    assert_eq!(
        gists::load(&state, &large.gist.id, None)
            .await
            .unwrap()
            .snapshot
            .sha,
        large.snapshot.sha
    );
    let (_, model) = a.model(&format!("/gists/{}/edit", large.gist.id)).await;
    let codeyard_webui::app::Page::Gists(
        codeyard_api_rest::user::fine_grained::gists::Page::Editor(editor),
    ) = model
    else {
        panic!("expected editor")
    };
    assert!(editor.files[0].content.is_none());
    let count = f.count().await;
    for files in [
        BTreeMap::from([("a.txt".into(), "x".repeat(MAX_GIST_BYTES + 1))]),
        (0..101)
            .map(|index| (format!("{index}.txt"), "x".into()))
            .collect(),
    ] {
        assert!(gists::create(
            state.clone(),
            alice,
            ca.clone(),
            CreateGist {
                description: String::new(),
                is_public: false,
                files
            }
        )
        .await
        .is_err());
        assert_eq!(f.count().await, count);
    }
    let hundred = gists::create(
        state.clone(),
        alice,
        ca.clone(),
        CreateGist {
            description: String::new(),
            is_public: false,
            files: (0..100)
                .map(|index| (format!("{index}.txt"), "x".into()))
                .collect(),
        },
    )
    .await
    .unwrap();
    assert_eq!(hundred.snapshot.files.len(), 100);
    let binary = gists::create(
        state.clone(),
        alice,
        ca,
        CreateGist {
            description: String::new(),
            is_public: false,
            files: BTreeMap::from([("binary.txt".into(), "\0binary".into())]),
        },
    )
    .await
    .unwrap();
    let (_, body) = request(
        &router,
        "gists/get",
        "GET",
        &format!("/api/v3/gists/{}", binary.gist.id),
        None,
        json!(null),
        200,
    )
    .await;
    assert_eq!(body["files"]["binary.txt"]["encoding"], "base64");
    assert_eq!(body["files"]["binary.txt"]["content"], "AGJpbmFyeQ==");
    // A creator may disconnect before learning a newly allocated ID. Their own list must
    // reconcile initial publications rather than leave a saved gist permanently invisible.
    for published in [false, true] {
        let unknown = uuid::Uuid::new_v4().simple().to_string();
        let backing = lifecycle::provision_gist(
            (&state.pool, None),
            &state.repos_path,
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
                actor_id: alice,
                target: codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(
                    alice,
                ),
            },
            &format!("gist-{unknown}"),
            None,
        )
        .await
        .unwrap();
        codeyard_db_repos::gists::reserve(
            f.pool(),
            alice,
            &AccountCredential::Session(&a.session),
            &codeyard_db_repos::gists::NewGist {
                repo_id: backing.id,
                id: &unknown,
                description: "Disconnected creator",
                is_public: false,
            },
        )
        .await
        .unwrap();
        let sha = {
            let git = git2::Repository::open_bare(&backing.disk_path).unwrap();
            let mut builder = git.treebuilder(None).unwrap();
            builder
                .insert(
                    "recovered.txt",
                    git.blob(b"recovered initial publication\n").unwrap(),
                    0o100644,
                )
                .unwrap();
            let tree = git.find_tree(builder.write().unwrap()).unwrap();
            let author = git2::Signature::now("alice", "alice@users.noreply").unwrap();
            git.commit(
                None,
                &author,
                &author,
                "Interrupted initial save",
                &tree,
                &[],
            )
            .unwrap()
            .to_string()
        };
        codeyard_db_repos::gists::admit(
            f.pool(),
            alice,
            &AccountCredential::Session(&a.session),
            &codeyard_db_repos::gists::Admission {
                repo_id: backing.id,
                disk_path: &backing.disk_path,
                storage_node_id: Some(None),
                base: None,
                sha: &sha,
                description: None,
            },
        )
        .await
        .unwrap();
        if published {
            publish(
                Path::new(&backing.disk_path),
                &sha,
                "0000000000000000000000000000000000000000",
            );
        }
        assert!(
            !codeyard_db_repos::gists::get(f.pool(), &unknown)
                .await
                .unwrap()
                .unwrap()
                .ready
        );
        let listing = a.page_ok("/gists?tab=mine").await;
        if published {
            assert!(listing.contains(&unknown));
            assert!(
                codeyard_db_repos::gists::get(f.pool(), &unknown)
                    .await
                    .unwrap()
                    .unwrap()
                    .ready
            );
        } else {
            assert!(codeyard_db_repos::gists::get(f.pool(), &unknown)
                .await
                .unwrap()
                .is_none());
            assert!(!Path::new(&backing.disk_path).exists());
        }
    }
    assert!(repo_store(repo, None)
        .gist_snapshot(f.pool(), repo.id, Some(original))
        .await
        .is_ok());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_gist_authority() {
    authority(false).await
}
#[tokio::test]
async fn postgres_gist_authority() {
    authority(true).await
}
