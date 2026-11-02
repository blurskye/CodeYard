use super::*;
use codeyard::backend::{ci::tokens::Permissions, db};
use std::collections::BTreeMap;

async fn access(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("package-actions").await;
    let bob = f.actor("package-other").await;
    f.execute("INSERT INTO repositories(owner_user_id,name,disk_path,is_private) VALUES($1,'workflow','package-workflow.git',true),($1,'different','package-different.git',true)",&[alice]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_user_id=$1 AND name='workflow'",
            &[alice],
        )
        .await;
    let run_id = if pg {
        "'a00ca9c0-131e-4157-a7e5-12b353b069d8'"
    } else {
        "X'a00ca9c0131e4157a7e512b353b069d8'"
    };
    f.execute(&format!("INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,event,branch,head_sha,workflow_path,worker,status) VALUES({run_id},$1,$2,'local','push','main','0123456789012345678901234567890123456789','.github/workflows/publish.yml','package-worker','running')"),&[repo,alice]).await;
    let run = f
        .scalar(
            "SELECT id FROM ci_runs WHERE repo_id=$1 AND worker='package-worker'",
            &[repo],
        )
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = format!("Bearer {}", token(&f, &router, alice, "write").await);
    let outsider = format!("Bearer {}", token(&f, &router, bob, "write").await);
    let policies: Vec<_> = [("writer", 2), ("reader", 1), ("none", 0)]
        .into_iter()
        .map(|(name, level)| {
            let value = format!("ghs_package_{name}");
            // Every job token carries metadata read, as workflow permissions always add it.
            let permissions = serde_json::to_string(&Permissions(BTreeMap::from([
                ("packages".into(), level),
                ("contents".into(), 2),
                ("metadata".into(), 1),
            ])))
            .unwrap();
            (name, tokens::token_hash(&value), permissions)
        })
        .collect();
    let credentials: Vec<_> = policies
        .iter()
        .map(|(job, hash, permissions)| db::ci_tokens::NewJobToken {
            job,
            hash,
            permissions,
            write: true,
        })
        .collect();
    db::ci_tokens::issue(f.pool(), run, "package-worker", &credentials)
        .await
        .unwrap();
    let job_writer = "Bearer ghs_package_writer";
    let job_reader = "Bearer ghs_package_reader";
    let registry_reader = exchange(
        &router,
        Some(job_reader),
        "repository:package-actions/workflow:pull",
    )
    .await;
    let no_packages = "Bearer ghs_package_none";
    // A run may create an owner package only when it remains linked to that run's repository.
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let digest = push_blob(&router, job_writer, "package-actions/workflow", config).await;
    let media = "application/vnd.oci.image.manifest.v1+json";
    let manifest = json!({"schemaVersion":2,"mediaType":media,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":digest,"size":config.len()},"layers":[]}).to_string().into_bytes();
    let path = "/v2/package-actions/workflow/manifests/latest";
    let (status, _, body) = request(
        &router,
        "PUT",
        path,
        Some(job_writer),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _) = request(&router, "GET", path, Some(job_reader), media, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(
        &router,
        "PUT",
        path,
        Some(job_reader),
        media,
        manifest.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) =
        request(&router, "GET", path, Some(no_packages), media, vec![], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    push_blob(&router, &writer, "package-actions/different", config).await;
    let (status, _, _) = request(
        &router,
        "GET",
        &format!("/v2/package-actions/different/blobs/{digest}"),
        Some(job_reader),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let api = "/api/v3/users/package-actions/packages/container/workflow";
    let (status, _, body) = request(
        &router,
        "GET",
        api,
        Some(job_reader),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    crate::support::github_spec::assert_github_schema("packages/get-package-for-user", 200, &body);
    assert_eq!(body["repository"]["id"], repo);
    for (method, suffix, operation, expected) in [
        ("DELETE", "", "packages/delete-package-for-user", 204),
        ("POST", "/restore", "packages/restore-package-for-user", 204),
    ] {
        let (status, _, body) = request(
            &router,
            method,
            &format!("{api}{suffix}"),
            Some(job_writer),
            "application/json",
            vec![],
            None,
        )
        .await;
        assert_eq!(
            status.as_u16(),
            expected,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert!(body.is_empty(), "{operation}");
    }
    let (status, _, _) = request(
        &router,
        "GET",
        path,
        Some(&registry_reader),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Public repository reads do not grant access to a private package.
    f.execute(
        "UPDATE repositories SET is_private=false WHERE id=$1",
        &[repo],
    )
    .await;
    let (status, _, _) = request(&router, "GET", path, Some(&outsider), media, vec![], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Account and run revocations are checked again, not cached with a registry login.
    f.execute("DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=$1)",&[run]).await;
    let (status, _, _) = request(&router, "GET", path, Some(job_reader), media, vec![], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = request(
        &router,
        "GET",
        path,
        Some(&registry_reader),
        media,
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Transferring a repository does not strand its package or grant it to the new organization.
    f.execute(
        "INSERT INTO organizations(name) VALUES('package-transferred')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='package-transferred'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner'),($1,$3,'admin')",
        &[org, alice, bob],
    )
    .await;
    db::repositories::records::transfer_repository(
        f.pool(),
        alice,
        repo,
        db::repositories::creation::RepositoryTarget::Organization(org),
    )
    .await
    .unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM packages WHERE repo_id=$1", &[repo])
            .await,
        0
    );
    let (status, _, _) = request(&router, "GET", path, Some(&writer), media, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(&router, "GET", path, Some(&outsider), media, vec![], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.execute("UPDATE users SET is_banned=true WHERE id=$1", &[alice])
        .await;
    let (status, _, _) = request(&router, "PUT", path, Some(&writer), media, manifest, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    f.finish().await;
}

#[tokio::test]
async fn packages_access_sqlite() {
    access(false).await;
}
#[tokio::test]
async fn packages_access_postgres() {
    access(true).await;
}
