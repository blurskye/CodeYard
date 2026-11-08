//! One advisory dataset for every cell: each cell pulls the same OSV mirror and matches its own
//! repositories, so a new advisory raises Dependabot alerts in both cells.
use super::cells_frontend::Server;
use crate::support::native_database::PostgresDatabase;
use codeyard_types::cells::Prefix;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

struct Cell {
    server: Server,
    state: Arc<codeyard_app::app::AppState>,
    owner: String,
    token: String,
    _root: tempfile::TempDir,
}

async fn cell(database: &PostgresDatabase, name: &str) -> Cell {
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, name, None)
        .await
        .unwrap();
    let session = codeyard_identity::sessions::fresh_id(&database.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &database.pool,
        &session,
        user.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = database
        .pool
        .cell_prefix()
        .token(codeyard_types::tokens::PAT_PREFIX, &"d".repeat(64))
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        &database.pool,
        user.id,
        &session,
        "advisories",
        &codeyard_support::token_hash::token_hash(&token),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(
        codeyard_storage::ObjectStore::open(root.path(), None)
            .await
            .unwrap(),
    );
    let state = codeyard_app::app::AppState::new(
        database.pool.clone(),
        root.path().into(),
        storage,
        None,
        Default::default(),
        None,
    );
    let app = codeyard_api_rest::create_router(state.clone());
    Cell {
        server: Server::start(app).await,
        state: Arc::new(state),
        owner: name.into(),
        token,
        _root: root,
    }
}

async fn api(cell: &Cell, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}/api/v3{path}", cell.server.url()))
        .bearer_auth(&cell.token)
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30));
    if !body.is_null() {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.bytes().await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn a_new_advisory_raises_alerts_in_both_cells() {
    use base64::Engine;
    let zero_database = PostgresDatabase::new().await;
    let one_database = PostgresDatabase::new().await;
    codeyard_db_core::cells::initialize(&one_database.pool, Prefix::new(1))
        .await
        .unwrap();
    let cells = [
        cell(&zero_database, "alice").await,
        cell(&one_database, "bob").await,
    ];
    let lock = "version = 3\n[[package]]\nname = \"example\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    for cell in &cells {
        let (status, created) = api(
            cell,
            reqwest::Method::POST,
            "/user/repos",
            json!({"name": "locked", "auto_init": true}),
        )
        .await;
        assert_eq!(status, 201, "{created}");
        let path = format!("/repos/{}/locked/contents/Cargo.lock", cell.owner);
        let (status, body) = api(
            cell,
            reqwest::Method::PUT,
            &path,
            json!({"message": "Lock a dependency", "content": base64::engine::general_purpose::STANDARD.encode(lock)}),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        let path = format!("/repos/{}/locked/vulnerability-alerts", cell.owner);
        let (status, body) = api(cell, reqwest::Method::PUT, &path, Value::Null).await;
        assert_eq!(status, 204, "{body}");
    }

    // The advisory is new to both cells: neither has an alert before the mirror has it.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers: Vec<_> = cells
        .iter()
        .map(|cell| {
            tokio::spawn(codeyard_app::vulnerabilities::run(
                cell.state.clone(),
                stopped.clone(),
            ))
        })
        .collect();
    let alerts = |cell: &Cell| format!("/repos/{}/locked/dependabot/alerts", cell.owner);
    tokio::time::sleep(Duration::from_secs(2)).await;
    for cell in &cells {
        let (status, listed) = api(cell, reqwest::Method::GET, &alerts(cell), Value::Null).await;
        assert_eq!(status, 200, "{listed}");
        assert_eq!(listed, json!([]), "{listed}");
    }

    let fixture = include_bytes!("../../backend/app/src/vulnerabilities/fixtures/OSV-2026-1.json");
    let archive = {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("OSV-2026-1.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(fixture).unwrap();
        zip.finish().unwrap().into_inner()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mirror = format!("http://{}", listener.local_addr().unwrap());
    let routes = axum::Router::new().route(
        "/crates.io/all.zip",
        axum::routing::get(move || {
            let archive = archive.clone();
            async move { archive }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, routes).await });
    let settings = codeyard_db_security::vulnerabilities::Settings {
        base_url: mirror,
        ecosystems: r#"["crates.io"]"#.into(),
    };
    for pool in [&zero_database.pool, &one_database.pool] {
        codeyard_app::vulnerabilities::synchronize(pool, &settings)
            .await
            .unwrap();
    }
    server.abort();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    for cell in &cells {
        loop {
            let (status, listed) =
                api(cell, reqwest::Method::GET, &alerts(cell), Value::Null).await;
            assert_eq!(status, 200, "{listed}");
            let found = listed.as_array().is_some_and(|alerts| {
                alerts.len() == 1
                    && alerts[0]["security_advisory"]["ghsa_id"] == "GHSA-2345-6789-cfgh"
                    && alerts[0]["state"] == "open"
            });
            if found {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} did not raise the alert: {listed}",
                cell.owner
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    stop.send_replace(true);
    for worker in workers {
        tokio::time::timeout(Duration::from_secs(30), worker)
            .await
            .unwrap()
            .unwrap();
    }
}
