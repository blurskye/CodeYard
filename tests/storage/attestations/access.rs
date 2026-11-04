use super::*;
use codeyard::backend::db;
use codeyard_model::permissions::Permissions;
use std::collections::BTreeMap;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("provenance-actions-owner").await;
    f.execute(
        "INSERT INTO organizations(name) VALUES('provenance-actions')",
        &[],
    )
    .await;
    let org = f
        .scalar(
            "SELECT id FROM organizations WHERE name='provenance-actions'",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    f.execute("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,is_private) VALUES('org',$1,'workflow','/provenance-actions/workflow.git',true),('org',$1,'other','/provenance-actions/other.git',true)",&[org]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_org_id=$1 AND name='workflow'",
            &[org],
        )
        .await;
    let other = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_org_id=$1 AND name='other'",
            &[org],
        )
        .await;
    for (index, id) in [(1, repo), (2, other)] {
        let run = if pg {
            format!("'abc00000-0000-4000-8000-00000000000{index}'")
        } else {
            format!("X'abc0000000004000800000000000000{index}'")
        };
        f.execute(&format!("INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,event,branch,head_sha,workflow_path,worker,status) VALUES({run},$1,$2,'local','push','main','0123456789012345678901234567890123456789','.github/workflows/provenance.yml','provenance-worker','running')"),&[id,alice]).await;
    }
    let run = f
        .scalar("SELECT id FROM ci_runs WHERE repo_id=$1", &[repo])
        .await;
    let other_run = f
        .scalar("SELECT id FROM ci_runs WHERE repo_id=$1", &[other])
        .await;
    let policies: Vec<_> = [("writer", 2), ("reader", 1), ("none", 0)]
        .into_iter()
        .map(|(name, level)| {
            let hash = tokens::token_hash(&format!("ghs_provenance_{name}"));
            let permissions = serde_json::to_string(&Permissions(BTreeMap::from([
                ("attestations".into(), level),
                ("artifact-metadata".into(), level),
                ("contents".into(), 2),
            ])))
            .unwrap();
            (name, hash, permissions)
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
    db::ci_tokens::issue(f.pool(), run, "provenance-worker", &credentials)
        .await
        .unwrap();
    let other_hash = tokens::token_hash("ghs_provenance_other");
    db::ci_tokens::issue(
        f.pool(),
        other_run,
        "provenance-worker",
        &[db::ci_tokens::NewJobToken {
            job: "other",
            hash: &other_hash,
            permissions: &policies[0].2,
            write: true,
        }],
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let owner = token(&f, &router, alice, "write").await;
    let digest = format!("sha256:{}", "f".repeat(64));
    let other_digest = format!("sha256:{}", "e".repeat(64));
    let attest = "/api/v3/repos/provenance-actions/workflow/attestations";
    let input = json!({"bundle":bundle(std::slice::from_ref(&digest),"https://slsa.dev/provenance/v1","CI signature")});
    call(
        &router,
        "repos/create-attestation",
        "POST",
        attest,
        Some("ghs_provenance_reader"),
        input.clone(),
        403,
    )
    .await;
    call(
        &router,
        "repos/create-attestation",
        "POST",
        attest,
        Some("ghs_provenance_writer"),
        input.clone(),
        201,
    )
    .await;
    let (_, read) = call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{attest}/{digest}"),
        Some("ghs_provenance_reader"),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(read["attestations"][0]["initiator"], "workflow");
    call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{attest}/{digest}"),
        Some("ghs_provenance_none"),
        Value::Null,
        403,
    )
    .await;
    call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("/api/v3/repos/provenance-actions/other/attestations/{digest}"),
        Some("ghs_provenance_reader"),
        Value::Null,
        404,
    )
    .await;
    let metadata = "/api/v3/orgs/provenance-actions/artifacts/metadata/storage-record";
    let input = json!({"name":"workflow artifact","digest":digest,"registry_url":"https://registry.example.test/","github_repository":"workflow"});
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        metadata,
        Some("ghs_provenance_writer"),
        input.clone(),
        200,
    )
    .await;
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        metadata,
        Some("ghs_provenance_reader"),
        input.clone(),
        403,
    )
    .await;
    let mut other_input = input.clone();
    other_input["digest"] = json!(other_digest);
    other_input["github_repository"] = json!("other");
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        metadata,
        Some(&owner),
        other_input.clone(),
        200,
    )
    .await;
    call(
        &router,
        "orgs/create-artifact-storage-record",
        "POST",
        metadata,
        Some("ghs_provenance_writer"),
        other_input,
        404,
    )
    .await;
    let (_, hidden) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!(
            "/api/v3/orgs/provenance-actions/artifacts/{other_digest}/metadata/storage-records"
        ),
        Some("ghs_provenance_reader"),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(hidden["total_count"], 0);
    let (_, read) = call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("/api/v3/orgs/provenance-actions/artifacts/{digest}/metadata/storage-records"),
        Some("ghs_provenance_reader"),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(read["total_count"], 1);
    // Live run revocation is not replaced by the bot account's grants on both repositories.
    f.execute("DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM actions_github_tokens WHERE run_id=$1)",&[run]).await;
    call(
        &router,
        "repos/list-attestations",
        "GET",
        &format!("{attest}/{digest}"),
        Some("ghs_provenance_reader"),
        Value::Null,
        401,
    )
    .await;
    call(
        &router,
        "orgs/list-artifact-storage-records",
        "GET",
        &format!("/api/v3/orgs/provenance-actions/artifacts/{digest}/metadata/storage-records"),
        Some("ghs_provenance_reader"),
        Value::Null,
        401,
    )
    .await;
    f.finish().await;
}

#[tokio::test]
async fn sqlite_scoped_provenance_credentials() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_scoped_provenance_credentials() {
    contract(true).await;
}
