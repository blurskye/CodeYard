//! Exact gh 2.102.0 gist and GPG-key requests through the application router.
use super::{
    gh_keys_auth::send,
    github_gpg::{armor, key},
};
use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};

use codeyard_identity::gpg;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::{json, Value};
use tower::ServiceExt;

const GIST_LIST: &str = "query GistList($endCursor:String$includeContent:Boolean!$per_page:Int!$visibility:GistPrivacy!){viewer{gists(first: $per_page, after: $endCursor, privacy: $visibility, orderBy: {field: CREATED_AT, direction: DESC}){nodes{description,files{name,text @include(if: $includeContent)},isPublic,name,updatedAt},pageInfo{hasNextPage,endCursor}}}}";

async fn listing(
    router: &Router,
    token: &str,
    privacy: &str,
    content: bool,
    cursor: Value,
) -> Value {
    let (_, value) = send(router, "POST", "/api/graphql", token, Some(json!({
        "query":GIST_LIST,"variables":{"endCursor":cursor,"includeContent":content,"per_page":1,"visibility":privacy}
    })), 200).await;
    assert!(value.get("errors").is_none(), "{value}");
    value["data"]["viewer"]["gists"].clone()
}

async fn raw(router: &Router, path: &str) -> (axum::http::HeaderMap, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status.as_u16(),
        200,
        "{path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    (headers, bytes.to_vec())
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("gh-alice").await;
    let bob = f.actor("gh-bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let mut credentials = Vec::new();
    for (actor, scope) in [(alice, "write"), (alice, "read"), (bob, "write")] {
        let session = Client::login(&router, f.pool(), actor).await;
        let issued = tokens::create(
            f.pool(),
            actor,
            Some(&session.session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("token {}", issued.value));
    }
    let write = &credentials[0];
    let read = &credentials[1];
    let other = &credentials[2];

    let input = json!({"description":"Public gist","public":true,"updated_at":"0001-01-01T00:00:00Z", "files":{
        "hello.txt":{"content":"hello\n","filename":"hello.txt","type":"text/plain","truncated":false}
    }});
    send(
        &router,
        "POST",
        "/api/v3/gists",
        read,
        Some(input.clone()),
        403,
    )
    .await;
    // gh prints "a gist file cannot be blank" on this 422.
    send(
        &router,
        "POST",
        "/api/v3/gists",
        write,
        Some(json!({"files":{"blank.txt":{"content":" \n"}}})),
        422,
    )
    .await;
    let (_, public) = send(&router, "POST", "/api/v3/gists", write, Some(input), 201).await;
    let id = public["id"].as_str().unwrap();
    assert!(public["html_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/gists/{id}")));
    let path = format!("/api/v3/gists/{id}");
    let (_, detail) = send(&router, "GET", &path, read, None, 200).await;
    assert_eq!(detail["id"], id);
    assert_eq!(detail["description"], "Public gist");
    assert_eq!(detail["owner"]["login"], "gh-alice");
    assert_eq!(detail["files"].as_object().unwrap().len(), 1);
    assert_eq!(detail["files"]["hello.txt"]["filename"], "hello.txt");
    assert_eq!(detail["files"]["hello.txt"]["content"], "hello\n");
    assert_eq!(detail["files"]["hello.txt"]["type"], "text/plain");
    assert_eq!(detail["files"]["hello.txt"]["truncated"], false);
    assert!(detail["files"]["hello.txt"]["raw_url"]
        .as_str()
        .unwrap()
        .contains("/raw/"));

    let (_, secret) = send(
        &router,
        "POST",
        "/api/v3/gists",
        write,
        Some(json!({
            "description":"Secret gist","public":false,"updated_at":"0001-01-01T00:00:00Z",
            "files":{"secret.txt":{"content":"private text"}}
        })),
        201,
    )
    .await;
    let secret_id = secret["id"].as_str().unwrap();
    let edit = json!({"description":"Edited public gist","files":{"hello.txt":{"content":"updated\n"},"extra.txt":{"filename":"extra.txt","content":"extra"}}});
    send(&router, "POST", &path, read, Some(edit.clone()), 403).await;
    send(&router, "POST", &path, other, Some(edit.clone()), 404).await;
    send(&router, "POST", &path, write, Some(edit), 200).await;

    // Updating the older gist must not move it ahead of the newer one in gh's picker.
    let first = listing(&router, read, "ALL", false, Value::Null).await;
    let node = &first["nodes"][0];
    assert_eq!(node["name"], secret_id);
    assert_eq!(node["description"], "Secret gist");
    assert_eq!(node["isPublic"], false);
    chrono::DateTime::parse_from_rfc3339(node["updatedAt"].as_str().unwrap()).unwrap();
    assert_eq!(node["files"], json!([{"name":"secret.txt"}]));
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    assert!(first["pageInfo"]["endCursor"].is_string());
    let second = listing(
        &router,
        read,
        "ALL",
        true,
        first["pageInfo"]["endCursor"].clone(),
    )
    .await;
    assert_eq!(second["pageInfo"]["hasNextPage"], false);
    assert!(second["pageInfo"]["endCursor"].is_string());
    assert_eq!(second["nodes"][0]["name"], id);
    assert_eq!(second["nodes"][0]["description"], "Edited public gist");
    assert_eq!(second["nodes"][0]["isPublic"], true);
    chrono::DateTime::parse_from_rfc3339(second["nodes"][0]["updatedAt"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        second["nodes"][0]["files"],
        json!([{"name":"extra.txt","text":"extra"},{"name":"hello.txt","text":"updated\n"}])
    );
    let public_list = listing(&router, read, "PUBLIC", false, Value::Null).await;
    assert_eq!(public_list["nodes"][0]["name"], id);
    assert_eq!(public_list["pageInfo"]["hasNextPage"], false);
    let secret_list = listing(&router, read, "SECRET", true, Value::Null).await;
    assert_eq!(secret_list["nodes"][0]["name"], secret_id);
    assert_eq!(
        secret_list["nodes"][0]["files"],
        json!([{"name":"secret.txt","text":"private text"}])
    );
    assert_eq!(secret_list["pageInfo"]["hasNextPage"], false);
    assert_eq!(
        listing(&router, other, "ALL", true, Value::Null).await["nodes"],
        json!([])
    );
    let (_, stranger) = send(&router,"POST","/api/graphql",other,Some(json!({"query":
        "{user(login:\"gh-alice\"){gists(first:100,privacy:ALL,orderBy:{field:CREATED_AT,direction:DESC}){nodes{name}}}}"
    })),200).await;
    assert!(stranger.get("errors").is_none(), "{stranger}");
    assert_eq!(
        stranger["data"]["user"]["gists"]["nodes"],
        json!([{"name":id}])
    );

    let (_, before_rename) = send(&router, "GET", &path, write, None, 200).await;
    let mut renamed = before_rename["files"]["hello.txt"].clone();
    renamed["filename"] = json!("renamed.txt");
    send(&router,"POST",&path,write,Some(json!({
        "description":"Edited public gist","public":false,"updated_at":"0001-01-01T00:00:00Z",
        "files":{"hello.txt":{"content":""},"renamed.txt":renamed,"extra.txt":before_rename["files"]["extra.txt"]}
    })),200).await;
    let (_, after_rename) = send(&router, "GET", &path, write, None, 200).await;
    assert!(after_rename["files"].get("hello.txt").is_none());
    assert_eq!(after_rename["files"]["renamed.txt"]["content"], "updated\n");
    assert_eq!(
        after_rename["public"], true,
        "gh's zero public field must not change visibility"
    );
    send(&router,"POST",&path,write,Some(json!({"description":"Edited public gist","files":{"extra.txt":null,"renamed.txt":{"content":""}}})),200).await;
    let (_, cleared) = send(&router, "GET", &path, write, None, 200).await;
    assert_eq!(cleared["files"].as_object().unwrap().len(), 1);
    assert_eq!(cleared["files"]["renamed.txt"]["content"], "");

    let large = "a".repeat(1024 * 1024 + 64);
    let (_, truncated) = send(&router,"POST","/api/v3/gists",write,Some(json!({
        "description":"Large file","public":true,"updated_at":"0001-01-01T00:00:00Z","files":{"large.txt":{"content":large}}
    })),201).await;
    let large_id = truncated["id"].as_str().unwrap();
    let (_, truncated) = send(
        &router,
        "GET",
        &format!("/api/v3/gists/{large_id}"),
        read,
        None,
        200,
    )
    .await;
    assert_eq!(truncated["files"]["large.txt"]["truncated"], true);
    let raw_url =
        url::Url::parse(truncated["files"]["large.txt"]["raw_url"].as_str().unwrap()).unwrap();
    assert_eq!(raw(&router, raw_url.path()).await.1, large.as_bytes());

    // gh clone ID uses the singular GHES namespace, for HTTP and SSH alike.
    let (_, advertisement) = raw(
        &router,
        &format!("/gist/{id}.git/info/refs?service=git-upload-pack"),
    )
    .await;
    assert!(advertisement.starts_with(b"001e# service=git-upload-pack\n"));
    let resolved =
        codeyard_db_repos::repositories::records::get_git_repository(f.pool(), "gist", id)
            .await
            .unwrap()
            .unwrap();
    assert!(resolved.is_gist);
    assert_eq!(
        resolved.id,
        codeyard_db_repos::gists::get(f.pool(), id)
            .await
            .unwrap()
            .unwrap()
            .repo_id
    );
    send(&router, "DELETE", &path, read, None, 403).await;
    send(&router, "DELETE", &path, write, None, 204).await;
    send(&router, "GET", &path, write, None, 404).await;
    assert!(
        codeyard_db_repos::repositories::records::get_git_repository(f.pool(), "gist", id)
            .await
            .unwrap()
            .is_none()
    );

    for name in [None, Some("Laptop signing")] {
        let secret_key = key("alice@example.com");
        let armored = armor(&secret_key);
        let parsed = gpg::parse(&armored).unwrap();
        let mut input = json!({"armored_public_key":armored});
        if let Some(name) = name {
            input["name"] = json!(name);
        }
        send(
            &router,
            "POST",
            "/api/v3/user/gpg_keys",
            read,
            Some(input.clone()),
            403,
        )
        .await;
        send(
            &router,
            "POST",
            "/api/v3/user/gpg_keys",
            write,
            Some(input.clone()),
            201,
        )
        .await;
        let (_, list) = send(
            &router,
            "GET",
            "/api/v3/user/gpg_keys?per_page=100",
            read,
            None,
            200,
        )
        .await;
        let keys = list.as_array().unwrap();
        assert_eq!(keys.len(), 1);
        let listed = &keys[0];
        assert_eq!(listed["key_id"], parsed.key_id);
        assert_eq!(listed["public_key"], parsed.material.public_key);
        assert_eq!(listed["created_at"], json!(parsed.material.created_at));
        assert_eq!(listed["expires_at"], json!(parsed.material.expires_at));
        assert_eq!(
            listed["emails"]
                .as_array()
                .unwrap()
                .iter()
                .map(|email| email["email"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["alice@example.com", "work@example.com"]
        );
        chrono::DateTime::parse_from_rfc3339(listed["created_at"].as_str().unwrap()).unwrap();
        let (_, duplicate) = send(
            &router,
            "POST",
            "/api/v3/user/gpg_keys",
            write,
            Some(input),
            422,
        )
        .await;
        assert_eq!(duplicate["errors"][0]["field"], "key_id");
        assert_eq!(duplicate["errors"][0]["message"], "key_id already exists");
        let database_id = keys
            .iter()
            .find(|key| key["key_id"] == parsed.key_id)
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        assert!(database_id > 0);
        let path = format!("/api/v3/user/gpg_keys/{database_id}");
        send(&router, "DELETE", &path, read, None, 403).await;
        send(&router, "DELETE", &path, other, None, 404).await;
        send(&router, "DELETE", &path, write, None, 204).await;
        assert_eq!(
            send(
                &router,
                "GET",
                "/api/v3/user/gpg_keys?per_page=100",
                write,
                None,
                200
            )
            .await
            .1,
            json!([])
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_gists_gpg() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_gh_gists_gpg() {
    contract(true).await;
}
