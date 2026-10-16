use super::fixture::{Fixture, COLLISION, PRIVATE, USER};
use axum::http::StatusCode;
use codeyard::{backend::db, webui::app::Page};

const UNAVAILABLE: &str = "This page's data is temporarily unavailable. Please try again.";

async fn sanitized(f: &Fixture, path: &str, authenticated: bool) {
    let reply = f.page(path, authenticated).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    match reply.envelope.page {
        Page::Error {
            status, message, ..
        } => {
            assert_eq!(status, 503);
            assert_eq!(message, UNAVAILABLE);
        }
        other => panic!("expected sanitized unavailable model, got {other:?}"),
    }
    for forbidden in [
        PRIVATE,
        "not-executed.git",
        "discovery_zz_invalid",
        "unmatched_invalid",
        "owner_user_id",
        "owner_org_id",
        "repo_owner_check",
        "SELECT ",
        "UPDATE ",
    ] {
        assert!(
            !reply.body.contains(forbidden),
            "public response leaked {forbidden}"
        );
    }
}

pub async fn run(f: &Fixture) {
    let repo = f
        .database
        .repository("user", "discovery_zz_invalid", false)
        .await;
    // Last in both personal-name and public/search order: valid user/org entries
    // already filled the cache. Invalid ownership must still fail before a hit.
    f.database.execute("UPDATE repositories SET stars_count=-100,created_at='1990-01-01T00:00:00.000000Z',updated_at='1990-01-01T00:00:00.000000Z' WHERE id=$1", &[repo]).await;
    let shapes = [
        ("user", Some(COLLISION), Some(COLLISION)),
        ("org", Some(COLLISION), Some(COLLISION)),
        ("user", None, None),
        ("org", None, None),
        ("user", None, Some(COLLISION)),
        ("org", Some(COLLISION), None),
        ("unknown", Some(COLLISION), None),
        ("USER", Some(COLLISION), None),
    ];
    for (kind, user, org) in shapes {
        // The current migrated schema rejects these writes. Fixture-only CHECK
        // relaxation admits the ownership write to exercise legacy-row reads.
        let rejected = f
            .database
            .owner_shape(repo, kind, user, org)
            .await
            .unwrap_err();
        let code = rejected.as_database_error().and_then(|error| error.code());
        assert!(matches!(code.as_deref(), Some("23514" | "275")));
        assert_eq!(
            db::repositories::records::repository_owner_name(f.database.pool(), repo)
                .await
                .unwrap()
                .as_deref(),
            Some(USER)
        );
        f.database.historical_shape(repo, kind, user, org).await;
        assert_eq!(
            db::repositories::records::repository_owner_name(f.database.pool(), repo)
                .await
                .unwrap(),
            None
        );
        for authenticated in [false, true] {
            sanitized(f, "/explore", authenticated).await;
            sanitized(f, "/search?q=discovery", authenticated).await;
        }
        sanitized(f, "/", true).await;
        for path in ["/api/repos", "/api/search?q=discovery"] {
            let (status, error) = f.api(path).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
            assert_eq!(error["error"], "unavailable");
        }
        f.database.restore_owner_check(repo).await;
    }
    // Valid kind/shape with an absent identity is rejected by current FKs; no
    // foreign-key disabling, dangling production row or repair path is introduced.
    for (kind, user, org) in [
        ("user", Some(900_000_000), None),
        ("org", None, Some(900_000_000)),
    ] {
        let rejected = f
            .database
            .owner_shape(repo, kind, user, org)
            .await
            .unwrap_err();
        let code = rejected.as_database_error().and_then(|error| error.code());
        assert!(matches!(code.as_deref(), Some("23503" | "787")));
        assert_eq!(
            db::repositories::records::repository_owner_name(f.database.pool(), repo)
                .await
                .unwrap()
                .as_deref(),
            Some(USER)
        );
    }
    // Search's issue-only path also shares the kind-aware cache. The repository
    // no longer matches repository search, but its malformed issue does.
    f.database
        .execute(
            "UPDATE repositories SET name='unmatched_invalid' WHERE id=$1",
            &[repo],
        )
        .await;
    f.database.execute("INSERT INTO issues(repo_id,number,author_id,title,body) VALUES($1,1,700000,'discovery broken issue','Fixture')", &[repo]).await;
    f.database
        .historical_shape(repo, "org", Some(COLLISION), Some(COLLISION))
        .await;
    // The repository matches again through its owner's name, which contains "discovery".
    assert_eq!(
        db::repositories::discovery::search_repositories(f.database.pool(), "discovery", 50)
            .await
            .unwrap()
            .len(),
        5
    );
    sanitized(f, "/search?q=discovery&type=issues", false).await;
    sanitized(f, "/search?q=discovery&type=issues", true).await;
    let (status, error) = f.api("/api/search?q=discovery&type=issues").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert_eq!(error["error"], "unavailable");
    f.database.restore_owner_check(repo).await;
}
