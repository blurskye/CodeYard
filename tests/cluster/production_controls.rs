//! Persistent account/IP counters must be shared by independently composed web nodes.
use crate::support::database::Database;
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Request, StatusCode},
    response::Response,
    Router,
};
use tower::ServiceExt;

async fn get(router: &Router, token: Option<&str>, address: &str) -> Response {
    let mut request = Request::builder()
        .uri(if token.is_some() {
            "/api/v3/user"
        } else {
            "/api/v3/meta"
        })
        .extension(ConnectInfo(
            address.parse::<std::net::SocketAddr>().unwrap(),
        ))
        // Neither of these client-supplied addresses may escape stamping.
        .header("x-forwarded-for", "192.0.2.99")
        .header(
            codeyard_identity::client_address::CLIENT_HEADER,
            "192.0.2.99",
        );
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn assert_last_slot(first: Response, second: Response, limit: &str) {
    let mut codes = [first.status(), second.status()];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::FORBIDDEN]);
    for response in [first, second] {
        assert_eq!(response.headers()["x-ratelimit-limit"], limit);
        assert_eq!(response.headers()["x-ratelimit-remaining"], "0");
        if response.status() == StatusCode::FORBIDDEN {
            assert!(response.headers().contains_key("retry-after"));
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["message"], "API rate limit exceeded");
        }
    }
}

#[tokio::test]
async fn account_and_ip_limits_are_atomic_across_web_nodes_on_both_engines() {
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let first = db.router(roots[0].path()).await;
        let second = db.router(roots[1].path()).await;
        let user = db.actor("shared-rate-user").await;
        let token_a = super::drain::token(&db, user).await;
        let token_b = super::drain::token(&db, user).await;
        assert_eq!(
            get(&first, Some(&token_a), "192.0.2.1:1000").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&second, Some(&token_b), "192.0.2.2:1001")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(db.scalar("SELECT used FROM github_api_limits WHERE requester LIKE 'user:%' AND resource='core'", &[]).await, 2);
        // Put this fixture one request short of the real hourly account budget.
        db.execute("UPDATE github_api_limits SET used=4999 WHERE requester LIKE 'user:%' AND resource='core'", &[]).await;
        let (a, b) = tokio::join!(
            get(&first, Some(&token_a), "192.0.2.1:1000"),
            get(&second, Some(&token_b), "192.0.2.2:1001")
        );
        assert_last_slot(a, b, "5000").await;
        assert_eq!(
            get(&first, None, "192.0.2.1:1000").await.status(),
            StatusCode::OK
        );
        db.execute(
            "UPDATE github_api_limits SET used=59 WHERE requester LIKE 'ip:%' AND resource='core'",
            &[],
        )
        .await;
        let (a, b) = tokio::join!(
            get(&first, None, "192.0.2.1:1000"),
            get(&second, None, "192.0.2.1:1001")
        );
        assert_last_slot(a, b, "60").await;
        assert_eq!(
            get(&second, None, "192.0.2.2:1001").await.status(),
            StatusCode::OK
        );
        db.execute("UPDATE users SET is_banned=true WHERE id=$1", &[user])
            .await;
        assert_eq!(
            get(&second, Some(&token_a), "192.0.2.3:1002")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        db.finish().await;
    }
}

#[tokio::test]
async fn drain_and_audit_are_atomic_and_audits_recheck_admin_access_across_web_nodes() {
    use crate::support::http_client::Client;
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let first = db.router(roots[0].path()).await;
        let second = db.router(roots[1].path()).await;
        let user = db.actor("shared-audit-admin").await;
        db.execute("UPDATE users SET is_admin=true WHERE id=$1", &[user])
            .await;
        let writer = Client::login(&first, db.pool(), user).await;
        let reader = Client {
            router: second,
            session: writer.session.clone(),
            csrf: writer.csrf.clone(),
        };
        let node = super::drain::node(&db, "audited-node", 9000).await;
        assert_eq!(
            writer
                .form_reply("/admin/nodes/audited-node/drain", &[])
                .await
                .status,
            StatusCode::SEE_OTHER
        );
        let page = reader.page_ok("/admin/audit?action=node.drain").await;
        assert!(page.contains("node.drain") && page.contains("shared-audit-admin"));
        assert_eq!(db.scalar("SELECT COUNT(*) FROM audit_log WHERE action='node.drain' AND actor_id=$1 AND target_id=$2", &[user, node]).await, 1);
        assert_eq!(
            writer
                .form_reply("/admin/nodes/audited-node/cancel-drain", &[])
                .await
                .status,
            StatusCode::SEE_OTHER
        );
        if pg {
            db.execute("CREATE FUNCTION owned_drain_audit_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='node.drain' THEN RAISE EXCEPTION 'owned audit outage'; END IF; RETURN NEW; END $$", &[]).await;
            db.execute("CREATE TRIGGER owned_drain_audit_failure BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION owned_drain_audit_failure()", &[]).await;
        } else {
            db.execute("CREATE TRIGGER owned_drain_audit_failure BEFORE INSERT ON audit_log WHEN NEW.action='node.drain' BEGIN SELECT RAISE(ABORT,'owned audit outage'); END", &[]).await;
        }
        assert_eq!(
            writer
                .form_reply("/admin/nodes/audited-node/drain", &[])
                .await
                .status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            codeyard_db_cluster::cluster::get_node_by_id(db.pool(), node)
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM audit_log WHERE action='node.drain'",
                &[]
            )
            .await,
            1
        );
        db.execute("UPDATE users SET is_admin=false WHERE id=$1", &[user])
            .await;
        let (status, body) = reader.page("/admin/audit?action=node.drain").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!body.contains("owned audit outage"));
        db.finish().await;
    }
}
