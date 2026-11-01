use super::*;
use codeyard::backend::{artifact_metadata, db, identity::tokens::token_hash};
use std::time::Duration;

async fn terminal(router: &Router, path: &str, reader: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (_, value) = call(
                router,
                "orgs/get-cluster-deployment-records-job",
                "GET",
                path,
                Some(reader),
                Value::Null,
                200,
            )
            .await;
            if matches!(value["status"].as_str(), Some("completed" | "failed")) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("background inventory job did not finish: {path}"))
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("inventory-owner").await;
    let bob = f.actor("inventory-writer").await;
    let carol = f.actor("inventory-outsider").await;
    f.execute(
        "INSERT INTO organizations(name) VALUES('inventory-org'),('unrelated-org')",
        &[],
    )
    .await;
    let unrelated = f
        .scalar(
            "SELECT id FROM organizations WHERE name='unrelated-org'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[unrelated, alice],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='inventory-org'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'public','/inventory/public.git',false),('org',$1,'private','/inventory/private.git',true)",&[org]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_org_id=$1 AND name='public'",
            &[org],
        )
        .await;
    f.execute(
        "INSERT INTO repo_access(repo_id,user_id,access_level) VALUES($1,$2,3)",
        &[repo, bob],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = token(&f, &router, alice, "write").await;
    let reader = token(&f, &router, alice, "read").await;
    let limited = token(&f, &router, bob, "write").await;
    let outsider = token(&f, &router, carol, "write").await;
    let a = format!("sha256:{}", "a".repeat(64));
    let b = format!("sha256:{}", "b".repeat(64));
    let c = format!("sha256:{}", "c".repeat(64));
    let d = format!("sha256:{}", "d".repeat(64));
    let e = format!("sha256:{}", "e".repeat(64));
    let base = "/api/v3/orgs/inventory-org/artifacts";
    let storage_path = format!("{base}/metadata/storage-record");
    let storage = json!({"name":"library","version":"1.0","digest":a,"registry_url":"https://registry.example.test/","repository":"releases","artifact_url":"https://registry.example.test/releases/library","github_repository":"public"});
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&reader),
        storage.clone(),
        403,
    )
    .await;
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&outsider),
        storage.clone(),
        403,
    )
    .await;
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&limited),
        storage.clone(),
        200,
    )
    .await;
    assert_eq!(stored["total_count"], 1);
    let id = stored["storage_records"][0]["id"].clone();
    let mut repeated = storage.clone();
    repeated["name"] = json!("ignored rename");
    repeated["version"] = json!("2.0");
    repeated["status"] = json!("eol");
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&writer),
        repeated.clone(),
        200,
    )
    .await;
    assert_eq!(stored["storage_records"][0]["id"], id);
    assert_eq!(stored["storage_records"][0]["name"], "library");
    assert_eq!(stored["storage_records"][0]["version"], "1.0");
    assert_eq!(stored["storage_records"][0]["status"], "eol");
    repeated["return_records"] = json!(false);
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&writer),
        repeated,
        200,
    )
    .await;
    assert!(stored.get("storage_records").is_none());
    let (_, listed) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("{base}/{a}/metadata/storage-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(listed["total_count"], 1);
    for (field, value) in [
        ("digest", json!("sha256:bad")),
        ("registry_url", json!("http://registry.test")),
        ("status", json!("unknown")),
        ("name", json!("")),
    ] {
        let mut invalid = storage.clone();
        invalid[field] = value;
        call(
            &router,
            "orgs/create-artifact-storage-record",
            "POST",
            &storage_path,
            Some(&writer),
            invalid,
            422,
        )
        .await;
    }
    let deployment_path = format!("{base}/metadata/deployment-record");
    let deployment = json!({"name":"deployed library","digest":a,"version":"1.0","github_repository":"public","status":"deployed","logical_environment":"prod","physical_environment":"east","cluster":"single","deployment_name":"pod","tags":{"team":"platform"},"runtime_risks":["internet-exposed"]});
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        &deployment_path,
        Some(&writer),
        deployment.clone(),
        200,
    )
    .await;
    let id = stored["deployment_records"][0]["id"].clone();
    let mut update = deployment.clone();
    update["digest"] = json!(b);
    update["version"] = json!("2.0");
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        &deployment_path,
        Some(&writer),
        update,
        200,
    )
    .await;
    assert_eq!(stored["deployment_records"][0]["id"], id);
    let (_, old) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{a}/metadata/deployment-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(old["total_count"], 0);
    let (_, new) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{b}/metadata/deployment-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(new["deployment_records"][0]["id"], id);
    // The repository supplied by a publisher cannot override a stored provenance association.
    let provenance = json!({"bundle":bundle(std::slice::from_ref(&c),"https://slsa.dev/provenance/v1","provenance signature")});
    for name in ["public", "private"] {
        call(
            &router,
            "repos/create-attestation",
            "POST",
            &format!("/api/v3/repos/inventory-org/{name}/attestations"),
            Some(&writer),
            provenance.clone(),
            201,
        )
        .await;
    }
    let mut proven = deployment.clone();
    proven["digest"] = json!(c);
    proven["deployment_name"] = json!("private-pod");
    proven["github_repository"] = json!("does-not-exist");
    let (_, stored) = call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        &deployment_path,
        Some(&writer),
        proven.clone(),
        200,
    )
    .await;
    assert!(stored["deployment_records"][0]["attestation_id"].is_i64());
    call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        &deployment_path,
        Some(&limited),
        proven,
        404,
    )
    .await;
    let (_, hidden) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{c}/metadata/deployment-records"),
        Some(&limited),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hidden["total_count"], 0);
    let mut provenance_storage = storage.clone();
    provenance_storage["digest"] = json!(c);
    provenance_storage["github_repository"] = json!("does-not-exist");
    let (_, both) = call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&writer),
        provenance_storage.clone(),
        200,
    )
    .await;
    assert_eq!(both["total_count"], 2);
    // An inaccessible association must not let another repository poison this publisher's inventory.
    let (_, own) = call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&limited),
        provenance_storage,
        200,
    )
    .await;
    assert_eq!(own["total_count"], 1);
    let mut own_deployment = deployment.clone();
    own_deployment["digest"] = json!(c);
    own_deployment["deployment_name"] = json!("limited-pod");
    own_deployment["github_repository"] = json!("does-not-exist");
    let (_, own) = call(
        &router,
        "orgs/create-artifact-deployment-record",
        "POST",
        &deployment_path,
        Some(&limited),
        own_deployment,
        200,
    )
    .await;
    assert_ne!(
        own["deployment_records"][0]["attestation_id"],
        stored["deployment_records"][0]["attestation_id"]
    );
    let (_, visible) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("{base}/{c}/metadata/storage-records"),
        Some(&limited),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(visible["total_count"], 1);
    let (_, elsewhere) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("/api/v3/orgs/unrelated-org/artifacts/{c}/metadata/storage-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(elsewhere["total_count"], 0);
    let cluster_path = format!("{base}/metadata/deployment-record/cluster/fleet");
    let deploy_d = json!({"name":"first","digest":d,"deployment_name":"first-pod","github_repository":"public"});
    let deploy_e = json!({"name":"second","digest":e,"deployment_name":"second-pod","github_repository":"public"});
    let batch = json!({"logical_environment":"prod","deployments":[deploy_d,deploy_e]});
    let (_, two) = call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        batch.clone(),
        200,
    )
    .await;
    assert_eq!(two["total_count"], 2);
    let (_, one) = call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        json!({"logical_environment":"prod","deployments":[batch["deployments"][0]]}),
        200,
    )
    .await;
    assert_eq!(one["total_count"], 1);
    let (_, omitted) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{e}/metadata/deployment-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(omitted["total_count"], 1);
    call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        batch.clone(),
        200,
    )
    .await;
    let mut mixed = batch.clone();
    mixed["deployments"][1]["github_repository"] = json!("missing");
    call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        mixed.clone(),
        404,
    )
    .await;
    mixed["partial_success"] = json!(true);
    let (_, partial) = call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        mixed.clone(),
        207,
    )
    .await;
    assert_eq!(partial["total_count"], 1);
    assert_eq!(partial["errors"][0]["cause"], "not_found");
    let mut rejected = mixed.clone();
    rejected["deployments"][0]["github_repository"] = json!("missing");
    let (_, no_op) = call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&writer),
        rejected.clone(),
        207,
    )
    .await;
    assert_eq!(no_op["total_count"], 0);
    let (_, retained) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{e}/metadata/deployment-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(retained["total_count"], 1);
    let no_op_jobs = format!("{cluster_path}/jobs");
    // A job whose every record is refused is refused as a whole, so a stranger's write token
    // records no job for this organization either.
    let count_jobs = "SELECT COUNT(*) FROM artifact_metadata_jobs";
    let before = f.scalar(count_jobs, &[]).await;
    for (credential, body) in [(&writer, &rejected), (&outsider, &rejected), (&outsider, &mixed)] {
        call(
            &router,
            "orgs/create-cluster-deployment-records-job",
            "POST",
            &no_op_jobs,
            Some(credential),
            body.clone(),
            403,
        )
        .await;
    }
    assert_eq!(f.scalar(count_jobs, &[]).await, before);
    let (_, empty) = call(
        &router,
        "orgs/set-cluster-deployment-records",
        "POST",
        &cluster_path,
        Some(&limited),
        json!({"logical_environment":"prod","deployments":[]}),
        200,
    )
    .await;
    assert_eq!(empty["total_count"], 0);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM artifact_metadata_records WHERE cluster='fleet'",
            &[]
        )
        .await,
        2
    );
    let (_, empty_job) = call(
        &router,
        "orgs/create-cluster-deployment-records-job",
        "POST",
        &no_op_jobs,
        Some(&limited),
        json!({"logical_environment":"prod","deployments":[]}),
        202,
    )
    .await;
    let (_, empty_job) = call(
        &router,
        "orgs/get-cluster-deployment-records-job",
        "GET",
        &format!("{no_op_jobs}/{}", empty_job["job_id"]),
        Some(&limited),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty_job["status"], "completed");
    assert_eq!(empty_job["total_count"], 0);
    let jobs = format!("{base}/metadata/deployment-record/cluster/background/jobs");
    let (_, queued) = call(
        &router,
        "orgs/create-cluster-deployment-records-job",
        "POST",
        &jobs,
        Some(&writer),
        mixed.clone(),
        202,
    )
    .await;
    assert_eq!(queued["errors"].as_array().unwrap().len(), 1);
    let path = format!("{jobs}/{}", queued["job_id"]);
    let (_, pending) = call(
        &router,
        "orgs/get-cluster-deployment-records-job",
        "GET",
        &path,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(pending["status"], "pending");
    call(
        &router,
        "orgs/create-cluster-deployment-records-job",
        "POST",
        &jobs,
        Some(&writer),
        mixed.clone(),
        409,
    )
    .await;
    call(
        &router,
        "orgs/get-cluster-deployment-records-job",
        "GET",
        &path,
        Some(&outsider),
        Value::Null,
        404,
    )
    .await;
    call(
        &router,
        "orgs/get-cluster-deployment-records-job",
        "GET",
        &path.replace("background/jobs", "wrong/jobs"),
        Some(&reader),
        Value::Null,
        404,
    )
    .await;
    // Two normal web workers resume an already committed queue and cannot apply a job twice.
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let first_worker = tokio::spawn(artifact_metadata::run(f.pool().clone(), receiver.clone()));
    let second_worker = tokio::spawn(artifact_metadata::run(f.pool().clone(), receiver));
    let completed = terminal(&router, &path, &reader).await;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["total_count"], 1);
    assert_eq!(completed["errors"].as_array().unwrap().len(), 1);
    stop.send_replace(true);
    first_worker.await.unwrap();
    second_worker.await.unwrap();
    // Stopping a consumer leaves any unfinished job pending without visible half-updates;
    // normal consumers resume it without duplicating inventory.
    let interrupted_jobs = format!("{base}/metadata/deployment-record/cluster/interrupted/jobs");
    let (_, interrupted) = call(
        &router,
        "orgs/create-cluster-deployment-records-job",
        "POST",
        &interrupted_jobs,
        Some(&writer),
        batch.clone(),
        202,
    )
    .await;
    let interrupted_path = format!("{interrupted_jobs}/{}", interrupted["job_id"]);
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(artifact_metadata::run(f.pool().clone(), receiver));
    tokio::task::yield_now().await;
    stop.send_replace(true);
    worker.await.unwrap();
    let (_, interrupted) = call(
        &router,
        "orgs/get-cluster-deployment-records-job",
        "GET",
        &interrupted_path,
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    if interrupted["status"] == "pending" {
        assert_eq!(
            f.scalar(
                "SELECT COUNT(*) FROM artifact_metadata_records WHERE cluster='interrupted'",
                &[]
            )
            .await,
            0
        );
    }
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(artifact_metadata::run(f.pool().clone(), receiver));
    assert_eq!(
        terminal(&router, &interrupted_path, &reader).await["total_count"],
        2
    );
    stop.send_replace(true);
    worker.await.unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM artifact_metadata_records WHERE cluster='interrupted'",
            &[]
        )
        .await,
        2
    );
    // Revoke a publisher after enqueueing: the worker rechecks the original credential and commits no inventory.
    let revokable = token(&f, &router, alice, "write").await;
    let jobs = format!("{base}/metadata/deployment-record/cluster/revoked/jobs");
    let (_, queued) = call(
        &router,
        "orgs/create-cluster-deployment-records-job",
        "POST",
        &jobs,
        Some(&revokable),
        batch.clone(),
        202,
    )
    .await;
    let token_id = db::users::tokens::find_token(f.pool(), &token_hash(&revokable))
        .await
        .unwrap()
        .unwrap()
        .id;
    f.execute(
        "DELETE FROM personal_access_tokens WHERE id=$1",
        &[token_id],
    )
    .await;
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(artifact_metadata::run(f.pool().clone(), receiver));
    let failed = terminal(&router, &format!("{jobs}/{}", queued["job_id"]), &reader).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM artifact_metadata_records WHERE cluster='revoked'",
            &[]
        )
        .await,
        0
    );
    stop.send_replace(true);
    worker.await.unwrap();
    // Removing one shared bundle association clears provenance links, not the inventory or the
    // bundle still owned by the other repository.
    for _ in 0..8 {
        call(
            &router,
            "repos/create-attestation",
            "POST",
            "/api/v3/repos/inventory-org/private/attestations",
            Some(&writer),
            provenance.clone(),
            201,
        )
        .await;
        let private_attestation = f.scalar("SELECT a.id FROM artifact_attestations a JOIN repositories r ON r.id=a.repo_id WHERE r.owner_org_id=$1 AND r.name='private'",&[org]).await;
        let delete_path = format!("/api/v3/orgs/inventory-org/attestations/{private_attestation}");
        let mut refresh = storage.clone();
        refresh["digest"] = json!(c);
        tokio::join!(
            call(
                &router,
                "orgs/delete-attestations-by-id",
                "DELETE",
                &delete_path,
                Some(&writer),
                Value::Null,
                204
            ),
            call(
                &router,
                "orgs/create-artifact-storage-record",
                "POST",
                &storage_path,
                Some(&writer),
                refresh,
                200
            )
        );
    }
    let (_, detached) = call(
        &router,
        "orgs/list-artifact-deployment-records",
        "GET",
        &format!("{base}/{c}/metadata/deployment-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert!(detached["deployment_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["deployment_name"] == "private-pod")
        .unwrap()["attestation_id"]
        .is_null());
    db::attestations::collect(f.pool()).await.unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM attestation_bundles", &[])
            .await,
        1
    );
    // Long Unicode coordinates stay indexable in PostgreSQL: stable storage identities are hashed.
    let mut unicode = storage.clone();
    unicode["path"] = json!("🦀".repeat(500));
    unicode["repository"] = json!("🦀".repeat(120));
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        &storage_path,
        Some(&writer),
        unicode,
        200,
    )
    .await;
    let destination = f
        .scalar(
            "SELECT id FROM organizations WHERE name='unrelated-org'",
            &[],
        )
        .await;
    db::repositories::records::transfer_repository(
        f.pool(),
        alice,
        repo,
        db::repositories::creation::RepositoryTarget::Organization(destination),
    )
    .await
    .unwrap();
    assert_eq!(f.scalar("SELECT COUNT(*) FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id WHERE a.repo_id=$1",&[repo]).await,0);
    let (_, detached) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("{base}/{a}/metadata/storage-records"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(detached["total_count"], 0);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_artifact_metadata_contract() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_artifact_metadata_contract() {
    contract(true).await;
}
