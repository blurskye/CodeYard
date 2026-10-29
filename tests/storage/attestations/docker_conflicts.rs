use super::*;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("docker-owner").await;
    let bob = f.actor("docker-reader").await;
    f.execute("INSERT INTO organizations(name) VALUES('docker-org')", &[])
        .await;
    let org = f
        .scalar("SELECT id FROM organizations WHERE name='docker-org'", &[])
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    f.execute("INSERT INTO packages(owner_kind,owner_user_id,name,package_type,visibility,created_at,updated_at) VALUES('user',$1,'image','container','public',1,1)",&[alice]).await;
    f.execute("INSERT INTO packages(owner_kind,owner_org_id,name,package_type,visibility,created_at,updated_at) VALUES('org',$1,'image','container','private',1,1)",&[org]).await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let reader = token(&f, &router, bob, "read").await;
    let oauth = oauth_token(&f, &router, bob, "read:packages").await;
    let wrong_scope = oauth_token(&f, &router, bob, "read:org").await;
    for (operation, path) in [
        (
            "packages/list-docker-migration-conflicting-packages-for-organization",
            "/api/v3/orgs/docker-org/docker/conflicts",
        ),
        (
            "packages/list-docker-migration-conflicting-packages-for-user",
            "/api/v3/users/docker-owner/docker/conflicts",
        ),
        (
            "packages/list-docker-migration-conflicting-packages-for-authenticated-user",
            "/api/v3/user/docker/conflicts",
        ),
    ] {
        call(&router, operation, "GET", path, None, Value::Null, 401).await;
        let (headers, rows) = call(
            &router,
            operation,
            "GET",
            path,
            Some(&reader),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(rows, json!([]));
        assert!(!headers.contains_key("link"));
        let (_, rows) = call(
            &router,
            operation,
            "GET",
            path,
            Some(&oauth),
            Value::Null,
            200,
        )
        .await;
        assert_eq!(rows, json!([]));
        call(
            &router,
            operation,
            "GET",
            path,
            Some(&wrong_scope),
            Value::Null,
            403,
        )
        .await;
    }
    call(
        &router,
        "packages/list-docker-migration-conflicting-packages-for-user",
        "GET",
        "/api/v3/users/no-such-user/docker/conflicts",
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    call(
        &router,
        "packages/list-docker-migration-conflicting-packages-for-organization",
        "GET",
        "/api/v3/orgs/no-such-org/docker/conflicts",
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_docker_conflicts() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_docker_conflicts() {
    contract(true).await;
}
