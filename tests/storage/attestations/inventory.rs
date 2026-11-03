use super::*;
use axum::http::StatusCode;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("inventory-browser-owner").await;
    f.execute(
        "INSERT INTO organizations(name) VALUES('inventory-browser')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='inventory-browser'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'public','/inventory-browser/public.git',false),('org',$1,'private','/inventory-browser/private.git',true)",&[org]).await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = token(&f, &router, alice, "write").await;
    let public_digest = format!("sha256:{}", "7".repeat(64));
    let private_digest = format!("sha256:{}", "8".repeat(64));
    for (name, digest) in [("public", &public_digest), ("private", &private_digest)] {
        call(&router,"repos/create-attestation","POST",&format!("/api/v3/repos/inventory-browser/{name}/attestations"),Some(&writer),json!({"bundle":bundle(std::slice::from_ref(digest),"https://slsa.dev/provenance/v1","browser fixture signature")}),201).await;
        call(&router,"orgs/create-artifact-storage-record","POST","/api/v3/orgs/inventory-browser/artifacts/metadata/storage-record",Some(&writer),json!({"name":format!("{name} artifact"),"digest":digest,"registry_url":"https://registry.example.test/","path":format!("{name}-storage-marker")}),200).await;
    }
    let owner = Client::login(&router, f.pool(), alice).await;
    let guest = Client::anonymous(&router);
    let base = "/orgs/inventory-browser/artifacts";
    let html = guest.page_ok(base).await;
    assert!(html.contains("Find artifact"));
    let html = guest
        .page_ok(&format!("{base}/{public_digest}?kind=storage"))
        .await;
    assert!(html.contains("public-storage-marker"));
    assert!(!html.contains(&writer));
    let html = guest
        .page_ok(&format!("{base}/{private_digest}?kind=storage"))
        .await;
    assert!(!html.contains("private-storage-marker"));
    assert!(!html.contains("inventory-browser/private"));
    let html = owner
        .page_ok(&format!("{base}/{private_digest}?kind=storage"))
        .await;
    assert!(html.contains("private-storage-marker"));
    let (_, model) = owner
        .model(&format!("{base}/{private_digest}?kind=storage"))
        .await;
    let codeyard_webui::app::Page::ArtifactMetadata(page) = model else {
        panic!("typed native inventory")
    };
    assert_eq!(page.entries.len(), 1);
    let rows: Vec<_> = (0..31).map(|id|json!({"name":"public artifact","digest":public_digest,"deployment_name":format!("public-pod-{id}")})).collect();
    let cluster =
        "/api/v3/orgs/inventory-browser/artifacts/metadata/deployment-record/cluster/public-fleet";
    call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        cluster,
        Some(&writer),
        json!({"logical_environment":"production","deployments":rows}),
        200,
    )
    .await;
    // Add a newer private provenance association and record for the same digest. It cannot create
    // an empty extra public page or expose a private record through the native model.
    call(&router,"repos/create-attestation","POST","/api/v3/repos/inventory-browser/private/attestations",Some(&writer),json!({"bundle":bundle(std::slice::from_ref(&public_digest),"https://slsa.dev/provenance/v1","private browser fixture")}),201).await;
    let tags: serde_json::Map<String, Value> = (0..12)
        .map(|id| {
            (
                format!("a{id:02}"),
                json!(format!("{}hidden-tag-tail", "x".repeat(210))),
            )
        })
        .collect();
    call(&router,"orgs/set-cluster-deployment-records","POST","/api/v3/orgs/inventory-browser/artifacts/metadata/deployment-record/cluster/private-fleet",Some(&writer),json!({"logical_environment":"production","deployments":[{"name":"private artifact","digest":public_digest,"deployment_name":"private-deployment-marker","tags":tags,"runtime_risks":["internet-exposed"]}]}),200).await;
    let (_, model) = guest.model(&format!("{base}/{public_digest}")).await;
    let codeyard_webui::app::Page::ArtifactMetadata(page) = model else {
        panic!("public inventory page")
    };
    assert_eq!(page.entries.len(), 30);
    assert!(page
        .entries
        .iter()
        .all(|entry| entry.repository == "inventory-browser/public"));
    let (_, model) = guest.model(page.next.as_deref().unwrap()).await;
    let codeyard_webui::app::Page::ArtifactMetadata(page) = model else {
        panic!("public inventory next page")
    };
    assert_eq!(page.entries.len(), 1);
    assert!(page.next.is_none());
    let (_, model) = owner
        .model(&format!("{base}/{public_digest}?after=30"))
        .await;
    let codeyard_webui::app::Page::ArtifactMetadata(page) = model else {
        panic!("private inventory preview")
    };
    let private = page
        .entries
        .iter()
        .find(|entry| entry.repository == "inventory-browser/private")
        .unwrap();
    assert_eq!(private.tags.len(), 10);
    assert_eq!(private.omitted_tags, 2);
    assert!(private
        .tags
        .iter()
        .all(|(_, value)| !value.contains("hidden-tag-tail")));
    assert!(private.bundle.is_some());
    let attack = json!({"name":"private artifact","digest":private_digest,"deployment_name":"private-xss-marker","status":"deployed","logical_environment":"staging","tags":{"untrusted":"</script><script>alert('inventory')</script>"}});
    call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        "/api/v3/orgs/inventory-browser/artifacts/metadata/deployment-record",
        Some(&writer),
        attack,
        200,
    )
    .await;
    let html = owner.page_ok(&format!("{base}/{private_digest}")).await;
    assert!(html.contains("private-xss-marker"));
    assert!(!html.contains("<script>alert('inventory')</script>"));
    assert_eq!(
        owner
            .form_reply(base, &[("digest", public_digest.as_str())])
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        guest.page(&format!("{base}/not-a-digest")).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        guest
            .page(&format!("{base}/{public_digest}?kind=unknown"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        guest
            .page(&format!("{base}/{public_digest}?after=-1"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let html = guest
        .page_ok("/inventory-browser/public/attestations")
        .await;
    assert!(html.contains(&format!("{base}/{public_digest}")));
    f.execute(
        "UPDATE repositories SET is_private=false WHERE owner_org_id=$1 AND name='private'",
        &[org],
    )
    .await;
    assert!(guest
        .page_ok(&format!("{base}/{private_digest}?kind=storage"))
        .await
        .contains("private-storage-marker"));
    f.execute(
        "UPDATE repositories SET is_private=true WHERE owner_org_id=$1 AND name='private'",
        &[org],
    )
    .await;
    assert!(!guest
        .page_ok(&format!("{base}/{private_digest}?kind=storage"))
        .await
        .contains("private-storage-marker"));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_native_artifact_inventory() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_native_artifact_inventory() {
    contract(true).await;
}
