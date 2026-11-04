//! gh issue/pr project documents from api/queries_projects_v2.go and queries_{repo,org}.go.
use super::{cli::send, *};
use codeyard_db_repos::projects as store;
use codeyard_identity::tokens;

macro_rules! document {
    ($name:literal) => {
        include_str!(concat!("queries/", $name, ".graphql"))
    };
}

async fn metadata_contract(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("project-metadata-alice").await;
    let bob = f.actor("project-metadata-bob").await;
    let alice_session = crate::support::live_session(f.pool(), alice).await;
    let bob_session = crate::support::live_session(f.pool(), bob).await;
    let alice_credential =
        codeyard_db_core::queries::users::accounts::AccountCredential::Session(&alice_session);
    let bob_credential =
        codeyard_db_core::queries::users::accounts::AccountCredential::Session(&bob_session);
    let org = applied(
        codeyard_db_orgs::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
                "project-metadata-org",
                codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    // Alice can read Bob's content without having write access to its repository.
    let repo = codeyard_repos_core::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(bob),
        "metadata",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let issue = codeyard_db_repos::issues::create_issue(f.pool(), repo.id, bob, "Issue", "")
        .await
        .unwrap();
    let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
        f.pool(),
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            repo_id: repo.id,
            head_repo_id: Some(repo.id),
            author_id: bob,
            title: "Pull",
            body: "",
            head_branch: "feature",
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "2222222222222222222222222222222222222222",
            is_draft: false,
            issue: None,
            maintainer_can_modify: false,
        },
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "project-metadata",
                    scope,
                    expires_at: None,
                },
            )
            .await
            .unwrap()
            .value,
        );
    }
    let token = &credentials[0];
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        alice,
        repo.id,
        codeyard_db_core::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    let mut projects = Vec::new();
    for owner in [
        store::Owner::User(alice),
        store::Owner::Organization(org.id),
    ] {
        for (title, public, closed) in [
            ("Z plan", true, false),
            ("A plan", true, false),
            ("Closed", true, true),
            ("Private", false, false),
        ] {
            let p = store::create(
                f.pool(),
                alice,
                &alice_credential,
                owner,
                title,
                None,
                public,
            )
            .await
            .unwrap();
            if closed {
                store::save(
                    f.pool(),
                    alice,
                    &alice_credential,
                    p.id,
                    &store::Settings {
                        title: Some(title.into()),
                        description: None,
                        public: Some(public),
                        closed: Some(closed),
                    },
                )
                .await
                .unwrap();
            }
            if public {
                store::link_repository(f.pool(), alice, &alice_credential, p.id, repo.id, true)
                    .await
                    .unwrap();
            }
            projects.push(p);
        }
    }
    assert!(
        codeyard_db_repos::repositories::access::revoke_repo_access(f.pool(), alice, repo.id)
            .await
            .unwrap()
    );
    let hidden = store::create(
        f.pool(),
        bob,
        &bob_credential,
        store::Owner::User(bob),
        "Hidden",
        None,
        false,
    )
    .await
    .unwrap();
    store::link_repository(f.pool(), bob, &bob_credential, hidden.id, repo.id, true)
        .await
        .unwrap();
    store::add_item(
        f.pool(),
        bob,
        &bob_credential,
        hidden.id,
        &store::Content::Issue(issue.id),
    )
    .await
    .unwrap();
    store::add_item(
        f.pool(),
        bob,
        &bob_credential,
        hidden.id,
        &store::Content::PullRequest(pull.id),
    )
    .await
    .unwrap();
    let unlinked = store::create(
        f.pool(),
        alice,
        &alice_credential,
        store::Owner::User(alice),
        "Unlinked",
        None,
        true,
    )
    .await
    .unwrap();
    for (text, key, owner, expected) in [
        (
            document!("RepositoryProjectV2List"),
            "repository",
            "project-metadata-bob",
            vec![
                projects[1].id,
                projects[5].id,
                projects[0].id,
                projects[4].id,
            ],
        ),
        (
            document!("OrganizationProjectV2List"),
            "organization",
            "project-metadata-org",
            vec![projects[5].id, projects[7].id, projects[4].id],
        ),
        (
            document!("UserProjectV2List"),
            "viewer",
            "project-metadata-alice",
            vec![projects[1].id, projects[3].id, unlinked.id, projects[0].id],
        ),
    ] {
        let vars = json!({"owner":owner,"name":"metadata","query":"is:open","endCursor":null});
        let data = send(&router, &credentials[1], text, vars.clone()).await;
        let connection = &data[key]["projectsV2"];
        assert_eq!(connection["pageInfo"]["hasNextPage"], false);
        let nodes = connection["nodes"].as_array().unwrap();
        assert_eq!(
            nodes
                .iter()
                .map(|n| n["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|id| codeyard_model::node_id::encode("PVT", *id))
                .collect::<Vec<_>>()
        );
        for n in nodes {
            assert_eq!(n["closed"], false);
            let project = projects
                .iter()
                .chain(std::iter::once(&unlinked))
                .find(|p| n["id"] == codeyard_model::node_id::encode("PVT", p.id))
                .unwrap();
            let prefix = if project.owner_kind == "org" {
                "/orgs/project-metadata-org/projects/"
            } else {
                "/users/project-metadata-alice/projects/"
            };
            assert_eq!(n["resourcePath"], format!("{prefix}{}", n["number"]));
            assert_eq!(
                n["url"],
                format!(
                    "http://localhost:3000{}",
                    n["resourcePath"].as_str().unwrap()
                )
            );
        }
        let mut after = vars.clone();
        after["endCursor"] = connection["pageInfo"]["endCursor"].clone();
        assert_eq!(
            send(&router, token, text, after).await[key]["projectsV2"]["nodes"],
            json!([])
        );
        // Exercise a non-final page without changing the rest of gh's selections.
        let small = text.replace("first: 100", "first: 1");
        let first = send(&router, token, &small, vars.clone()).await;
        assert_eq!(first[key]["projectsV2"]["pageInfo"]["hasNextPage"], true);
        let mut next = vars;
        next["endCursor"] = first[key]["projectsV2"]["pageInfo"]["endCursor"].clone();
        let second = send(&router, token, &small, next).await;
        assert_eq!(
            second[key]["projectsV2"]["nodes"][0]["id"],
            codeyard_model::node_id::encode("PVT", expected[1])
        );
    }
    // The named User field uses the same connection as viewer.
    let named = document!("UserProjectV2List")
        .replace("viewer {", "user(login: \"project-metadata-alice\") {");
    assert_eq!(
        send(&router, token, &named, json!({"query":"is:open A plan"})).await["user"]["projectsV2"]
            ["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (text, key, owner) in [
        (
            document!("RepositoryProjectList"),
            "repository",
            "project-metadata-bob",
        ),
        (
            document!("OrganizationProjectList"),
            "organization",
            "project-metadata-org",
        ),
    ] {
        let data = send(
            &router,
            token,
            text,
            json!({"owner":owner,"name":"metadata"}),
        )
        .await;
        assert_eq!(data[key]["projects"]["nodes"], json!([]));
        assert_eq!(
            data[key]["projects"]["pageInfo"],
            json!({"hasNextPage":false,"endCursor":null})
        );
    }
    let old_issue = store::add_item(
        f.pool(),
        alice,
        &alice_credential,
        projects[1].id,
        &store::Content::Issue(issue.id),
    )
    .await
    .unwrap();
    let old_pull = store::add_item(
        f.pool(),
        alice,
        &alice_credential,
        projects[5].id,
        &store::Content::PullRequest(pull.id),
    )
    .await
    .unwrap();
    let inputs = json!({
        "input_000":{"projectId":codeyard_model::node_id::encode("PVT",projects[0].id),"contentId":codeyard_model::node_id::encode("I",issue.id)},
        "input_001":{"projectId":codeyard_model::node_id::encode("PVT",projects[4].id),"contentId":codeyard_model::node_id::encode("PR",pull.id)},
        "input_002":{"projectId":codeyard_model::node_id::encode("PVT",projects[1].id),"itemId":codeyard_model::node_id::encode("PVTI",old_issue.id)},
        "input_003":{"projectId":codeyard_model::node_id::encode("PVT",projects[5].id),"itemId":codeyard_model::node_id::encode("PVTI",old_pull.id)},
    });
    let updated = send(
        &router,
        token,
        document!("UpdateProjectV2Items"),
        inputs.clone(),
    )
    .await;
    assert_eq!(
        updated["delete_002"]["deletedItemId"],
        inputs["input_002"]["itemId"]
    );
    assert_eq!(
        updated["delete_003"]["deletedItemId"],
        inputs["input_003"]["itemId"]
    );
    let mut new_items = Vec::new();
    for (project, content, alias) in [
        (&projects[0], store::Content::Issue(issue.id), "add_000"),
        (
            &projects[4],
            store::Content::PullRequest(pull.id),
            "add_001",
        ),
    ] {
        let items = store::items(f.pool(), Some(alice), project.id)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            updated[alias]["item"]["id"],
            codeyard_model::node_id::encode("PVTI", item.id)
        );
        assert!(match content {
            store::Content::Issue(id) => item.issue_id == Some(id),
            store::Content::PullRequest(id) => item.pull_request_id == Some(id),
            _ => false,
        });
        let fields = store::fields(f.pool(), Some(alice), project.id)
            .await
            .unwrap();
        let status = fields.iter().find(|f| f.name == "Status").unwrap();
        let config = store::field_configuration(status).unwrap();
        let option = &config["options"][1];
        store::update_item(
            f.pool(),
            alice,
            &alice_credential,
            project.id,
            item.id,
            &[store::FieldUpdate {
                id: status.id,
                value: option["id"].clone(),
            }],
        )
        .await
        .unwrap();
        store::archive_item(
            f.pool(),
            alice,
            &alice_credential,
            project.id,
            item.id,
            true,
        )
        .await
        .unwrap();
        new_items.push((item.id, option.clone()));
    }
    for (text, key, number, index) in [
        (document!("IssueProjectItems"), "issue", issue.number, 0),
        (
            document!("PullRequestProjectItems"),
            "pullRequest",
            pull.number,
            1,
        ),
    ] {
        let vars = json!({"owner":"project-metadata-bob","name":"metadata","number":number});
        let data = send(&router, &credentials[1], text, vars.clone()).await;
        let items = &data["repository"][key]["projectItems"];
        assert_eq!(items["totalCount"], 1);
        assert_eq!(
            items["nodes"][0]["id"],
            codeyard_model::node_id::encode("PVTI", new_items[index].0)
        );
        assert_eq!(
            items["nodes"][0]["status"],
            json!({"optionId":new_items[index].1["id"],"name":new_items[index].1["name"]})
        );
        assert_eq!(items["nodes"][0]["project"]["title"], "Z plan");
        assert_eq!(items["pageInfo"]["hasNextPage"], false);
        let mut next = vars;
        next["endCursor"] = items["pageInfo"]["endCursor"].clone();
        let data = send(&router, token, text, next).await;
        assert_eq!(data["repository"][key]["projectItems"]["totalCount"], 1);
        assert_eq!(data["repository"][key]["projectItems"]["nodes"], json!([]));
        let unset = store::add_item(
            f.pool(),
            alice,
            &alice_credential,
            projects[1 + index * 4].id,
            &if index == 0 {
                store::Content::Issue(issue.id)
            } else {
                store::Content::PullRequest(pull.id)
            },
        )
        .await
        .unwrap();
        let data = send(
            &router,
            token,
            text,
            json!({"owner":"project-metadata-bob","name":"metadata","number":number}),
        )
        .await;
        assert_eq!(data["repository"][key]["projectItems"]["totalCount"], 2);
        assert!(data["repository"][key]["projectItems"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |n| n["id"] == codeyard_model::node_id::encode("PVTI", unset.id)
                    && n["status"].is_null()
            ));
    }
    let mut denied_inputs = inputs;
    denied_inputs["input_002"] = json!({"projectId":codeyard_model::node_id::encode("PVT",projects[0].id),"itemId":codeyard_model::node_id::encode("PVTI",new_items[0].0)});
    denied_inputs["input_003"] = json!({"projectId":codeyard_model::node_id::encode("PVT",projects[4].id),"itemId":codeyard_model::node_id::encode("PVTI",new_items[1].0)});
    for credential in [&credentials[1], &credentials[2]] {
        let (_, body) = call(
            &router,
            Some(credential),
            document!("UpdateProjectV2Items"),
            denied_inputs.clone(),
        )
        .await;
        assert_eq!(body["errors"].as_array().unwrap().len(), 4, "{body}");
    }
    // Revoking content visibility refuses adding it, even to Alice's projects. Her projects'
    // existing items stay hers to remove, as ad1516496 lets a project writer do.
    f.execute(
        "UPDATE repositories SET is_private=true WHERE id=$1",
        &[repo.id],
    )
    .await;
    let (_, body) = call(
        &router,
        Some(token),
        document!("UpdateProjectV2Items"),
        denied_inputs.clone(),
    )
    .await;
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{body}");
    assert!(
        errors
            .iter()
            .all(|error| error["path"][0] == "add_000" || error["path"][0] == "add_001"),
        "{body}"
    );
    for alias in ["002", "003"] {
        assert_eq!(
            body["data"][format!("delete_{alias}")]["deletedItemId"],
            denied_inputs[format!("input_{alias}")]["itemId"],
            "{body}"
        );
    }
    for text in [
        document!("RepositoryProjectV2List"),
        document!("RepositoryProjectList"),
        document!("IssueProjectItems"),
        document!("PullRequestProjectItems"),
    ] {
        let (_, body) = call(
            &router,
            Some(token),
            text,
            json!({"owner":"project-metadata-bob","name":"metadata","number":1,"query":"is:open"}),
        )
        .await;
        assert_eq!(body["errors"][0]["type"], "NOT_FOUND", "{body}");
    }
    f.execute(
        "UPDATE repositories SET is_private=false WHERE id=$1",
        &[repo.id],
    )
    .await;
    for (project, (item, _)) in [projects[0].id, projects[4].id].into_iter().zip(new_items) {
        assert!(store::items(f.pool(), Some(alice), project)
            .await
            .unwrap()
            .iter()
            .all(|row| row.id != item));
    }
}

#[tokio::test]
async fn graphql_projects_metadata_sqlite() {
    metadata_contract(false).await;
}
#[tokio::test]
async fn graphql_projects_metadata_postgres() {
    metadata_contract(true).await;
}
