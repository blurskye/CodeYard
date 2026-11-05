use super::call;
use super::json;
use super::query;
use super::request;
use axum::{http::StatusCode, Router};
pub(super) async fn exercise(
    pool: &codeyard_db_core::DbPool,
    router: &Router,
    token: &str,
    bob: &str,
    repo: i64,
    actor: i64,
) {
    let milestone = codeyard_db_repos::milestones::create_milestone(
        pool,
        actor,
        repo,
        "GraphQL next",
        Some("Next release"),
        None,
    )
    .await
    .unwrap();
    let release = codeyard_db_repos::releases::create_release(
        pool,
        actor,
        repo,
        "graphql-v1",
        "main",
        &codeyard_db_repos::releases::ReleaseFields {
            name: Some("GraphQL release".into()),
            make_latest: true.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let file = codeyard_db_repos::files::reserve_upload(
        pool,
        codeyard_db_repos::files::ReserveUpload {
            user_id: actor,
            filename: "graphql.txt",
            content_type: "text/plain",
            file_size: 5,
            reason: codeyard_db_core::models::FileReason::ReleaseAsset,
            visibility: codeyard_db_core::models::FileVisibility::Owner,
            sha256_hex: &"a".repeat(64),
        },
    )
    .await
    .unwrap();
    assert!(
        codeyard_db_repos::files::verify_pending_upload(pool, file.id, actor)
            .await
            .unwrap()
    );
    let asset = codeyard_db_repos::releases::add_release_asset(
        pool,
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::support::live_session(pool, actor).await,
        ),
        repo,
        release.id,
        &codeyard_db_repos::releases::NewReleaseAsset {
            file_id: file.id,
            name: "graphql.txt",
            label: None,
            content_type: "text/plain",
            size: 5,
        },
    )
    .await
    .unwrap();
    let ids = [
        super::node_id::scoped("M", repo, milestone.id),
        super::node_id::scoped("RE", repo, release.id),
        super::node_id::scoped("RA", repo, asset.id),
    ];
    for (id, path) in [
        (
            ids[0].as_str(),
            format!(
                "/api/v3/repos/graphql-alice/proj/milestones/{}",
                milestone.number
            ),
        ),
        (
            ids[1].as_str(),
            format!("/api/v3/repos/graphql-alice/proj/releases/{}", release.id),
        ),
        (
            ids[2].as_str(),
            format!(
                "/api/v3/repos/graphql-alice/proj/releases/assets/{}",
                asset.id
            ),
        ),
    ] {
        let (status, rest) = request(router, Some(token), "GET", &path, json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        assert_eq!(rest["node_id"], id);
        let node = query(
            router,
            token,
            "query($id:ID!){node(id:$id){id __typename}}",
            json!({"id":rest["node_id"]}),
        )
        .await;
        assert_eq!(node["node"]["id"], id);
    }
    let document=query(router,token,"{repository(owner:\"graphql-alice\",name:\"proj\"){milestones(first:1,states:OPEN,query:\"GraphQL\",orderBy:{field:NUMBER,direction:ASC}){totalCount nodes{id title creator{login} progressPercentage}} releases(first:1,orderBy:{field:NAME,direction:ASC}){totalCount nodes{id name releaseAssets(first:10){totalCount nodes{id name downloadUrl}}}} latestRelease{id}}}",json!({})).await;
    assert_eq!(document["repository"]["milestones"]["totalCount"], 1);
    assert_eq!(
        document["repository"]["milestones"]["nodes"][0]["id"],
        ids[0]
    );
    assert_eq!(document["repository"]["latestRelease"]["id"], ids[1]);
    assert_eq!(
        document["repository"]["releases"]["nodes"][0]["releaseAssets"]["nodes"][0]["id"],
        ids[2]
    );
    for (_, id) in codeyard_db_repos::timeline::list_entries(pool, repo, None, false, false, 100, 0)
        .await
        .unwrap()
        .into_iter()
        .take(20)
    {
        let (status, rest) = request(
            router,
            Some(token),
            "GET",
            &format!("/api/v3/repos/graphql-alice/proj/issues/events/{id}"),
            json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        let value = query(
            router,
            token,
            "query($id:ID!){node(id:$id){id __typename}}",
            json!({"id":rest["node_id"]}),
        )
        .await;
        assert_eq!(value["node"]["id"], rest["node_id"]);
    }
    let (_, hidden) = call(
        router,
        Some(bob),
        "query($ids:[ID!]!){nodes(ids:$ids){id}}",
        json!({"ids":ids}),
    )
    .await;
    assert_eq!(hidden["data"]["nodes"], json!([null, null, null]));
    assert_eq!(hidden["errors"].as_array().unwrap().len(), 3);
}
