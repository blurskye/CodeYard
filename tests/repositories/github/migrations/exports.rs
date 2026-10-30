use super::*;
use codeyard::backend::db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};

async fn exported(router: &Router, path: &str, token: &str, operation: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let value = call(
                router,
                "GET",
                path,
                Some(token),
                Value::Null,
                operation,
                StatusCode::OK,
            )
            .await;
            assert_ne!(value["state"], "failed", "{value}");
            if value["state"] == "exported" {
                return value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

async fn raw(router: &Router, path: &str, token: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 101 * 1024 * 1024)
        .await
        .unwrap();
    if headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value == "application/json")
    {
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(
            "migrations/get-archive-for-authenticated-user",
            status.as_u16(),
            &value,
        );
    }
    (status, headers, bytes.to_vec())
}

pub(super) async fn run(
    router: &Router,
    f: &Database,
    root: &std::path::Path,
    actors: (i64, i64),
    credentials: (&str, &str, &str),
    personal: &db::models::Repository,
) {
    let (alice, bob) = actors;
    let (writer, reader, outsider) = credentials;
    let issue = call(
        router,
        "POST",
        "/api/v3/repos/import-owner/destination/issues",
        Some(writer),
        json!({"title":"Exported issue","body":"Original discussion"}),
        "issues/create",
        StatusCode::CREATED,
    )
    .await;
    call(
        router,
        "POST",
        &format!(
            "/api/v3/repos/import-owner/destination/issues/{}/comments",
            issue["number"]
        ),
        Some(writer),
        json!({"body":"Exported comment"}),
        "issues/create-comment",
        StatusCode::CREATED,
    )
    .await;
    call(
        router,
        "POST",
        "/api/v3/repos/import-owner/destination/labels",
        Some(writer),
        json!({"name":"exported","color":"c0ffee"}),
        "issues/create-label",
        StatusCode::CREATED,
    )
    .await;
    call(
        router,
        "POST",
        "/api/v3/repos/import-owner/destination/milestones",
        Some(writer),
        json!({"title":"Exported milestone"}),
        "issues/create-milestone",
        StatusCode::CREATED,
    )
    .await;
    call(
        router,
        "POST",
        "/api/v3/repos/import-owner/destination/releases",
        Some(writer),
        json!({"tag_name":"v1","name":"Exported release","body":"Original notes"}),
        "repos/create-release",
        StatusCode::CREATED,
    )
    .await;

    use crate::support::organizations::applied;
    use db::organizations as orgs;
    let org = applied(
        orgs::create_organization(
            f.pool(),
            alice,
            &orgs::NewOrganization::new(
                "migration-org",
                orgs::OrganizationProfile::new(None, Some("Migration metadata")).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_member(f.pool(), alice, org, bob, db::models::OrgRole::Member)
            .await
            .unwrap(),
    );
    let organization = lifecycle::provision(
        (f.pool(), None),
        root,
        RepositoryCreationOwner {
            actor_id: alice,
            target: RepositoryTarget::Organization(org),
        },
        "organization-source",
        None,
        true,
        None,
    )
    .await
    .unwrap();

    let team = applied(
        orgs::create_organization_team(
            f.pool(),
            alice,
            org,
            &orgs::NewTeam::new(
                "export-team",
                orgs::TeamProfile::new("Export team", None, orgs::TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        orgs::add_organization_team_member(f.pool(), alice, org, team, bob, orgs::TeamRole::Member)
            .await
            .unwrap(),
    );
    applied(
        orgs::grant_organization_team_repository(
            f.pool(),
            alice,
            org,
            team,
            organization.id,
            orgs::TeamRepositoryPermission::new(1).unwrap(),
        )
        .await
        .unwrap(),
    );

    let child = applied(
        orgs::create_organization_team(
            f.pool(),
            alice,
            org,
            &orgs::NewTeam::new(
                "export-child",
                orgs::TeamProfile::new("Export child", None, orgs::TeamPrivacy::Visible)
                    .unwrap()
                    .with_parent(Some(Some(team)))
                    .unwrap(),
            )
            .unwrap()
            .with_members_and_repositories(vec![alice], vec![]),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::create_organization_team(
            f.pool(),
            alice,
            org,
            &orgs::NewTeam::new(
                "export-secret",
                orgs::TeamProfile::new("Export secret", None, orgs::TeamPrivacy::Secret).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let page = orgs::OrganizationPage::new(0, 100).unwrap();
    let parent_members = applied(
        orgs::list_direct_organization_team_members(f.pool(), alice, org, team, page)
            .await
            .unwrap(),
    );
    assert_eq!(parent_members.len(), 1);
    assert_eq!(parent_members[0].user_id, bob);
    let child_members = applied(
        orgs::list_direct_organization_team_members(f.pool(), alice, org, child.id, page)
            .await
            .unwrap(),
    );
    assert_eq!(child_members.len(), 1);
    assert_eq!(child_members[0].user_id, alice);
    let parent_repos = applied(
        orgs::list_direct_organization_team_repositories(f.pool(), alice, org, team, page)
            .await
            .unwrap(),
    );
    assert_eq!(parent_repos.len(), 1);
    let child_repos = applied(
        orgs::list_direct_organization_team_repositories(f.pool(), alice, org, child.id, page)
            .await
            .unwrap(),
    );
    assert!(child_repos.is_empty());
    let mut projects = Vec::new();
    for owner in [
        db::projects::Owner::User(alice),
        db::projects::Owner::Organization(org),
    ] {
        projects.push(
            db::projects::create(
                f.pool(),
                alice,
                &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await),
                owner,
                "Migration project",
                Some("Private owner project"),
                false,
            )
            .await
            .unwrap(),
        );
    }
    db::projects::create(
        f.pool(),
        bob,
        &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), bob).await),
        db::projects::Owner::User(bob),
        "Unrelated private project",
        None,
        false,
    )
    .await
    .unwrap();
    // A project holds at most 100 fields, and Title and Status are built in.
    for n in 0..98 {
        db::projects::add_field(
            f.pool(),
            alice,
            &db::users::accounts::AccountCredential::Session(&crate::support::live_session(f.pool(), alice).await),
            projects[0].id,
            &db::projects::NewField {
                name: format!("Migration field {n}"),
                data_type: "text".into(),
                single_select_options: None,
                iteration_configuration: None,
            },
        )
        .await
        .unwrap();
    }
    assert!(db::projects::list_page(
        f.pool(),
        Some(bob),
        db::projects::Owner::User(alice),
        0,
        100
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        db::projects::fields_page(f.pool(), Some(bob), projects[0].id, 0, 100)
            .await
            .is_err()
    );
    assert!(db::projects::list_page(
        f.pool(),
        Some(alice),
        db::projects::Owner::User(alice),
        0,
        101
    )
    .await
    .is_err());
    projects[0] = db::projects::get(
        f.pool(),
        Some(alice),
        db::projects::Owner::User(alice),
        projects[0].number,
    )
    .await
    .unwrap();
    for (
        repo,
        owner,
        base,
        start_id,
        list_id,
        status_id,
        archive_id,
        delete_id,
        unlock_id,
        repos_id,
    ) in [
        (
            personal,
            "import-owner",
            "/api/v3/user/migrations",
            "migrations/start-for-authenticated-user",
            "migrations/list-for-authenticated-user",
            "migrations/get-status-for-authenticated-user",
            "migrations/get-archive-for-authenticated-user",
            "migrations/delete-archive-for-authenticated-user",
            "migrations/unlock-repo-for-authenticated-user",
            "migrations/list-repos-for-authenticated-user",
        ),
        (
            &organization,
            "migration-org",
            "/api/v3/orgs/migration-org/migrations",
            "migrations/start-for-org",
            "migrations/list-for-org",
            "migrations/get-status-for-org",
            "migrations/download-archive-for-org",
            "migrations/delete-archive-for-org",
            "migrations/unlock-repo-for-org",
            "migrations/list-repos-for-org",
        ),
    ] {
        call(
            router,
            "POST",
            base,
            Some(reader),
            json!({"repositories":[repo.name]}),
            start_id,
            StatusCode::FORBIDDEN,
        )
        .await;
        for input in [
            json!({}),
            json!({"repositories":[]}),
            json!({"repositories":["missing"]}),
            json!({"repositories":[repo.name,repo.name]}),
        ] {
            let value = call(
                router,
                "POST",
                base,
                Some(writer),
                input,
                start_id,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await;
            assert!(value["errors"].is_array());
        }
        let started = call(
            router,
            "POST",
            base,
            Some(writer),
            json!({"repositories":[repo.name],"lock_repositories":true}),
            start_id,
            StatusCode::CREATED,
        )
        .await;
        let path = format!("{base}/{}", started["id"]);
        let status = exported(router, &path, reader, status_id).await;
        assert_eq!(status["repositories"][0]["id"], repo.id);
        call(
            router,
            "GET",
            &path,
            Some(outsider),
            Value::Null,
            status_id,
            StatusCode::NOT_FOUND,
        )
        .await;
        let repositories = call(
            router,
            "GET",
            &format!("{path}/repositories?per_page=1"),
            Some(reader),
            Value::Null,
            repos_id,
            StatusCode::OK,
        )
        .await;
        assert_eq!(repositories[0]["id"], repo.id);
        let actor = db::users::accounts::get_user_by_id(f.pool(), alice)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !codeyard::backend::repositories::access::can_write(f.pool(), &actor, repo)
                .await
                .unwrap()
        );
        assert!(
            db::issues::create_issue(f.pool(), repo.id, alice, "Blocked", "Migration lock")
                .await
                .is_err()
        );
        assert!(repo_store(repo, None)
            .create_branch_as(f.pool(), repo.id, alice, "blocked", "trunk")
            .await
            .is_err());
        let archive_path = format!("{path}/archive");
        let (status, headers, bytes) = raw(router, &archive_path, reader).await;
        assert_eq!(status, StatusCode::FOUND);
        assert!(bytes.is_empty());
        let target = headers[header::LOCATION].to_str().unwrap();
        let target = url::Url::parse(target).unwrap();
        let target = target.path();
        let (status, _, _) = raw(router, target, outsider).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, headers, bytes) = raw(router, target, reader).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/gzip");
        assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
        assert!(!bytes.is_empty());
        let extracted = tempfile::tempdir().unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
            .unpack(extracted.path())
            .unwrap();
        let records: Value = serde_json::from_slice(
            &std::fs::read(extracted.path().join("repositories_000001.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(records[0]["type"], "repository");
        assert_eq!(records[0]["name"], repo.name);
        assert_eq!(records[0]["private"], true);
        let bare = git2::Repository::open_bare(
            extracted
                .path()
                .join(format!("repositories/{owner}/{}.git", repo.name)),
        )
        .unwrap();
        let actual: std::collections::HashMap<_, _> = bare
            .references()
            .unwrap()
            .map(|reference| {
                let reference = reference.unwrap();
                (
                    reference.name().unwrap().to_owned(),
                    reference.target().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(actual, repo_store(repo, None).mirror_refs().await.unwrap());
        // Exercise the reader and the authorized native publication path, not only tar extraction.
        let source = codeyard::backend::repositories::imports::archive::read(&bytes, None).unwrap();
        let prepared = codeyard::backend::repositories::imports::archive::git::prepare(&source)
            .await
            .unwrap();
        let user_refs: std::collections::HashMap<_, _> = actual
            .iter()
            .filter(|(name, _)| *name != "refs/codeyard" && !name.starts_with("refs/codeyard/"))
            .map(|(name, sha)| (name.clone(), sha.clone()))
            .collect();
        assert_eq!(prepared.refs, user_refs);
        assert!(prepared
            .refs
            .keys()
            .all(|name| !name.starts_with("refs/codeyard/")));
        let restored = lifecycle::provision(
            (f.pool(), None),
            root,
            RepositoryCreationOwner {
                actor_id: alice,
                target: repo.owner_org_id.map_or(
                    RepositoryTarget::Personal(alice),
                    RepositoryTarget::Organization,
                ),
            },
            "archive-git-restored",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        let generation = uuid::Uuid::new_v4().to_string();
        db::source_imports::start(
            f.pool(),
            alice,
            restored.id,
            "https://archive.test/snapshot.tar.gz",
            &generation,
        )
        .await
        .unwrap();
        let token = db::users::tokens::find_token(
            f.pool(),
            &codeyard::backend::identity::tokens::token_hash(writer.split_once(' ').unwrap().1),
        )
        .await
        .unwrap()
        .unwrap();
        repo_store(&restored, None)
            .import_git_as(
                f.pool(),
                restored.id,
                alice,
                codeyard::backend::repositories::imports::GitImport {
                    generation: generation.clone(),
                    credential: codeyard::backend::repositories::imports::ImportCredential::Token(
                        token.id,
                    ),
                    refs: prepared.refs,
                    default_branch: prepared.default_branch,
                },
                &prepared.pack,
            )
            .await
            .unwrap();
        db::source_imports::finish(f.pool(), restored.id, &generation, true)
            .await
            .unwrap();
        let store = repo_store(&restored, None);
        let mut imported = store.mirror_refs().await.unwrap();
        imported.retain(|name, _| name != "refs/codeyard" && !name.starts_with("refs/codeyard/"));
        assert_eq!(imported, user_refs);
        for (reference, sha) in &user_refs {
            if reference.starts_with("refs/heads/") {
                let proof = store
                    .branch_publication(restored.id, reference)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    proof.publication.actor,
                    codeyard::backend::git::PublicationActor::User(alice)
                );
                assert_eq!(proof.publication.repository_id, restored.id);
                assert_eq!(proof.publication.reference, *reference);
                assert_eq!(proof.publication.new, *sha);
                assert_eq!(proof.publication.old, "0".repeat(40));
            }
        }
        assert!(bare
            .config()
            .unwrap()
            .get_string("remote.origin.url")
            .is_err());
        let project = if repo.id == personal.id {
            &projects[0]
        } else {
            &projects[1]
        };
        let project_records: Value = serde_json::from_slice(
            &std::fs::read(extracted.path().join("projects_000001.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(project_records.as_array().unwrap().len(), 1);
        assert_eq!(project_records[0]["id"], project.id);
        assert_eq!(project_records[0]["public"], false);
        assert_eq!(project_records[0]["creator"]["id"], alice);
        assert_eq!(project_records[0]["created_at"], json!(project.created_at));
        assert_eq!(project_records[0]["updated_at"], json!(project.updated_at));
        let mut fields: Vec<Value> = Vec::new();
        for entry in std::fs::read_dir(extracted.path()).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("project_fields_")
            {
                fields.extend(
                    serde_json::from_slice::<Vec<Value>>(&std::fs::read(entry.path()).unwrap())
                        .unwrap(),
                );
            }
        }
        assert_eq!(fields.len(), if repo.id == personal.id { 100 } else { 2 });
        let status = fields
            .iter()
            .find(|field| field["name"] == "Status")
            .unwrap();
        let source_fields = db::projects::fields(f.pool(), Some(alice), project.id)
            .await
            .unwrap();
        let source_status = source_fields
            .iter()
            .find(|field| field.name == "Status")
            .unwrap();
        assert_eq!(
            status["source_configuration"],
            db::projects::field_configuration(source_status).unwrap()
        );
        assert_eq!(status["project"], project_records[0]["url"]);
        assert_eq!(status["created_at"], json!(source_status.created_at));
        if repo.id == personal.id {
            for (filename, field, text) in [
                ("issues_000001.json", "title", "Exported issue"),
                ("issue_comments_000001.json", "body", "Exported comment"),
                ("labels_000001.json", "name", "exported"),
                ("milestones_000001.json", "title", "Exported milestone"),
                ("releases_000001.json", "name", "Exported release"),
            ] {
                let records: Value = serde_json::from_slice(
                    &std::fs::read(extracted.path().join(filename)).unwrap(),
                )
                .unwrap();
                assert_eq!(records[0][field], text, "{filename}");
            }
        } else {
            let records: Value = serde_json::from_slice(
                &std::fs::read(
                    extracted
                        .path()
                        .join("organization_memberships_000001.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(records.as_array().unwrap().len(), 2);
            let records: Value = serde_json::from_slice(
                &std::fs::read(extracted.path().join("teams_000001.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0]["slug"], "export-team");
            assert_eq!(records[0]["privacy"], "closed");
            assert_eq!(records.as_array().unwrap().len(), 3);
            assert_eq!(records[0]["parent_team_id"], Value::Null);
            assert_eq!(records[1]["id"], child.id);
            assert_eq!(records[1]["parent_team_id"], team);
            assert_eq!(records[2]["privacy"], "secret");
            let records: Value = serde_json::from_slice(
                &std::fs::read(extracted.path().join("team_memberships_000001.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0]["user"]["login"], "import-outsider");
            assert_eq!(records.as_array().unwrap().len(), 1);
            let child_members: Value = serde_json::from_slice(
                &std::fs::read(extracted.path().join("team_memberships_000002.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(child_members.as_array().unwrap().len(), 1);
            assert_eq!(child_members[0]["user"]["id"], alice);
            let records: Value = serde_json::from_slice(
                &std::fs::read(extracted.path().join("team_repositories_000001.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0]["permission"], 1);
            let child_grants: Value = serde_json::from_slice(
                &std::fs::read(extracted.path().join("team_repositories_000002.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(child_grants, json!([]));
        }
        let unlock_path = format!("{path}/repos/{}/lock", repo.name);
        for (suffix, operation) in [(&archive_path, delete_id), (&unlock_path, unlock_id)] {
            call(
                router,
                "DELETE",
                suffix,
                Some(reader),
                Value::Null,
                operation,
                StatusCode::FORBIDDEN,
            )
            .await;
        }
        call(
            router,
            "DELETE",
            &archive_path,
            Some(writer),
            Value::Null,
            delete_id,
            StatusCode::NO_CONTENT,
        )
        .await;
        assert_eq!(raw(router, target, reader).await.0, StatusCode::NOT_FOUND);
        call(
            router,
            "GET",
            &archive_path,
            Some(reader),
            Value::Null,
            archive_id,
            StatusCode::NOT_FOUND,
        )
        .await;
        assert!(db::repository_exports::locked(f.pool(), repo.id)
            .await
            .unwrap());
        call(
            router,
            "DELETE",
            &unlock_path,
            Some(writer),
            Value::Null,
            unlock_id,
            StatusCode::NO_CONTENT,
        )
        .await;
        assert!(
            codeyard::backend::repositories::access::can_write(f.pool(), &actor, repo)
                .await
                .unwrap()
        );
        let second = call(
            router,
            "POST",
            base,
            Some(writer),
            json!({"repositories":[repo.name],"exclude_git_data":true,"exclude_metadata":true,"exclude":["repositories"]}),
            start_id,
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(second["repositories"], json!([]));
        assert_eq!(second["exclude"], json!(["repositories"]));
        exported(
            router,
            &format!("{base}/{}", second["id"]),
            reader,
            status_id,
        )
        .await;
        let (status, headers, values) = request(
            router,
            "GET",
            &format!("{base}?per_page=1&exclude[]=repositories"),
            Some(reader),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_github_schema(list_id, 200, &values);
        assert_eq!(values.as_array().unwrap().len(), 1);
        assert_eq!(values[0]["repositories"], json!([]));
        assert_eq!(values[0]["exclude"], json!(["repositories"]));
        assert!(headers[header::LINK]
            .to_str()
            .unwrap()
            .contains("rel=\"next\""));
        if repo.id == organization.id {
            call(
                router,
                "GET",
                base,
                Some(outsider),
                Value::Null,
                list_id,
                StatusCode::FORBIDDEN,
            )
            .await;
            for exclude in [false, true] {
                let metadata = call(router, "POST", base, Some(writer),
                    json!({"repositories":[],"org_metadata_only":true,"exclude_owner_projects":exclude}),
                    start_id, StatusCode::CREATED).await;
                exported(
                    router,
                    &format!("{base}/{}", metadata["id"]),
                    reader,
                    status_id,
                )
                .await;
                let (status, _, bytes) = raw(
                    router,
                    &format!(
                        "/api/v3/migration-archives/{}",
                        metadata["guid"].as_str().unwrap()
                    ),
                    reader,
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                let extracted = tempfile::tempdir().unwrap();
                tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
                    .unpack(extracted.path())
                    .unwrap();
                assert!(extracted.path().join("projects_000001.json").exists());
                assert!(extracted.path().join("project_fields_000001.json").exists());
                assert!(extracted.path().join("teams_000001.json").exists());
            }
        }
    }

    // Deleting another selected repository must not strand the surviving repository's lock.
    let deleted = lifecycle::provision(
        (f.pool(), None),
        root,
        RepositoryCreationOwner::personal(alice),
        "deleted-export-source",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let row = db::repository_exports::create(
        f.pool(),
        alice,
        None,
        &[personal.id, deleted.id],
        &db::repository_exports::ExportOptions {
            lock_repositories: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    repo_store(personal, None)
        .lock_migration_as(f.pool(), personal.id, alice, row.id)
        .await
        .unwrap();
    db::repositories::records::delete_repository(f.pool(), alice, deleted.id)
        .await
        .unwrap();
    assert!(db::repository_exports::by_guid(f.pool(), alice, &row.guid)
        .await
        .unwrap()
        .is_none());
    let path = format!("/api/v3/user/migrations/{}", row.id);
    let status = call(
        router,
        "GET",
        &path,
        Some(reader),
        Value::Null,
        "migrations/get-status-for-authenticated-user",
        StatusCode::OK,
    )
    .await;
    assert_eq!(status["repositories"].as_array().unwrap().len(), 1);
    call(
        router,
        "DELETE",
        &format!("{path}/repos/{}/lock", personal.name),
        Some(writer),
        Value::Null,
        "migrations/unlock-repo-for-authenticated-user",
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(!db::repository_exports::locked(f.pool(), personal.id)
        .await
        .unwrap());
    db::repository_exports::finish(f.pool(), row.id, "failed")
        .await
        .unwrap();
    call(
        router,
        "DELETE",
        &format!("{path}/archive"),
        Some(writer),
        Value::Null,
        "migrations/delete-archive-for-authenticated-user",
        StatusCode::NO_CONTENT,
    )
    .await;
}
