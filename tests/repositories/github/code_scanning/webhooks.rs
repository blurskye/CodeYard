use super::{call, db, json, sarif, Router, Value};
use codeyard::backend::code_scanning;

pub(super) async fn fixed_revision(
    router: &Router,
    pool: &codeyard_db_core::DbPool,
    hook: i64,
    token: &str,
    head: &str,
) {
    let previous = call(
        router,
        "GET",
        &format!("/api/v3/repos/alice/scan/git/commits/{head}"),
        token,
        Value::Null,
        "git/get-commit",
        200,
    )
    .await;
    let next = call(
        router,
        "POST",
        "/api/v3/repos/alice/scan/git/commits",
        token,
        json!({"message":"Scanner rule update","tree":previous["tree"]["sha"],"parents":[head]}),
        "git/create-commit",
        201,
    )
    .await;
    let fixed = next["sha"].as_str().unwrap();
    call(
        router,
        "PATCH",
        "/api/v3/repos/alice/scan/git/refs/heads/main",
        token,
        json!({"sha":fixed}),
        "git/update-ref",
        200,
    )
    .await;
    let endpoint = "/api/v3/repos/alice/scan/code-scanning/sarifs";
    call(
        router,
        "POST",
        endpoint,
        token,
        json!({"commit_sha":fixed,"ref":"refs/heads/main","sarif":sarif("semgrep/",false,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(pool).await.unwrap());
    let mut delivered = false;
    // The hook's host never resolves, so each delivery is recorded after its retries (7 s),
    // and a hook's deliveries go out in order: this one waits behind the six earlier events.
    for _ in 0..4500 {
        let deliveries = db::webhooks::list_deliveries(pool, hook).await.unwrap();
        for delivery in deliveries {
            let body: Value = serde_json::from_str(&delivery.request_body).unwrap();
            if delivery.event == "code_scanning_alert" && body["action"] == "fixed" {
                assert_eq!(body["ref"], "refs/heads/main");
                assert_eq!(
                    body["commit_oid"], fixed,
                    "a fixed event describes the fixing analysis, not the last finding's commit"
                );
                assert_eq!(body["alert"]["most_recent_instance"]["commit_sha"], head);
                delivered = true;
            }
        }
        if delivered {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(delivered, "fixed webhook must be enqueued");
    call(
        router,
        "PATCH",
        "/api/v3/repos/alice/scan/git/refs/heads/main",
        token,
        json!({"sha":head,"force":true}),
        "git/update-ref",
        200,
    )
    .await;
    call(
        router,
        "POST",
        endpoint,
        token,
        json!({"commit_sha":head,"ref":"refs/heads/main","sarif":sarif("semgrep/",true,1)}),
        "code-scanning/upload-sarif",
        202,
    )
    .await;
    assert!(code_scanning::process_next(pool).await.unwrap());
}
