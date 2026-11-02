use super::action_secrets::{pat, request};
use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::db;
use serde_json::{json, Value};

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let writer = pat(&f, &router, alice, "write").await;
    let reader = pat(&f, &router, alice, "read").await;
    let outsider = pat(&f, &router, bob, "write").await;
    let base = "/api/v3/repos/alice/project/actions/variables";
    gh_empty_value(&router, base, base, &reader, &writer, None).await;
    for (name, value) in [
        ("region", "eu-west"),
        ("SECOND", "line 1\n'line 2' $VALUE 🦀"),
    ] {
        request(
            &router,
            "actions/create-repo-variable",
            "POST",
            base,
            Some(&writer),
            json!({"name":name,"value":value}),
            201,
        )
        .await;
    }
    request(
        &router,
        "actions/create-repo-variable",
        "POST",
        base,
        Some(&writer),
        json!({"name":"REGION","value":"duplicate"}),
        409,
    )
    .await;
    let (headers, list) = request(
        &router,
        "actions/list-repo-variables",
        "GET",
        &format!("{base}?per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["variables"][0]["name"], "REGION");
    assert_eq!(list["variables"][0]["value"], "eu-west");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, second) = request(
        &router,
        "actions/list-repo-variables",
        "GET",
        &format!("{base}?page=2&per_page=1"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(second["variables"][0]["name"], "SECOND");
    let (_, variable) = request(
        &router,
        "actions/get-repo-variable",
        "GET",
        &format!("{base}/region"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(variable, list["variables"][0]);
    let gh_list = super::action_secrets::gh_list(
        &router,
        "actions/list-repo-variables",
        base,
        &reader,
        "variables",
    )
    .await;
    assert_eq!(gh_list["variables"][0], variable);
    assert_eq!(gh_list["variables"][1], second["variables"][0]);
    request(
        &router,
        "actions/update-repo-variable",
        "PATCH",
        &format!("{base}/region"),
        Some(&writer),
        json!({"name":"new_region"}),
        204,
    )
    .await;
    let (_, renamed) = request(
        &router,
        "actions/get-repo-variable",
        "GET",
        &format!("{base}/NEW_REGION"),
        Some(&reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(renamed["created_at"], variable["created_at"]);
    assert_eq!(renamed["value"], "eu-west");
    request(
        &router,
        "actions/update-repo-variable",
        "PATCH",
        &format!("{base}/NEW_REGION"),
        Some(&writer),
        json!({"value":""}),
        204,
    )
    .await;
    request(
        &router,
        "actions/update-repo-variable",
        "PATCH",
        &format!("{base}/NEW_REGION"),
        Some(&writer),
        json!({"name":"SECOND"}),
        422,
    )
    .await;
    for (method, path, operation, token, expected, body) in [
        (
            "GET",
            base.to_owned(),
            "actions/list-repo-variables",
            Some(outsider.as_str()),
            404,
            Value::Null,
        ),
        (
            "GET",
            base.to_owned(),
            "actions/list-repo-variables",
            None,
            404,
            Value::Null,
        ),
        (
            "GET",
            format!("{base}/MISSING"),
            "actions/get-repo-variable",
            Some(reader.as_str()),
            404,
            Value::Null,
        ),
        (
            "POST",
            base.to_owned(),
            "actions/create-repo-variable",
            Some(reader.as_str()),
            403,
            json!({"name":"NO_WRITE","value":"bad"}),
        ),
        (
            "PATCH",
            format!("{base}/SECOND"),
            "actions/update-repo-variable",
            Some(reader.as_str()),
            403,
            json!({"value":"bad"}),
        ),
        (
            "DELETE",
            format!("{base}/SECOND"),
            "actions/delete-repo-variable",
            Some(reader.as_str()),
            403,
            Value::Null,
        ),
        (
            "PATCH",
            format!("{base}/MISSING"),
            "actions/update-repo-variable",
            Some(writer.as_str()),
            404,
            json!({"value":"bad"}),
        ),
    ] {
        request(&router, operation, method, &path, token, body, expected).await;
    }
    for input in [
        json!({"name":"GITHUB_TOKEN","value":"bad"}),
        json!({"name":"BAD-NAME","value":"bad"}),
        json!({"name":"NUL","value":"bad\0value"}),
        json!({"name":"LARGE","value":"x".repeat(49153)}),
        json!({"value":"missing name"}),
    ] {
        request(
            &router,
            "actions/create-repo-variable",
            "POST",
            base,
            Some(&writer),
            input,
            422,
        )
        .await;
    }
    db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        repo.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    request(
        &router,
        "actions/list-repo-variables",
        "GET",
        base,
        Some(&outsider),
        Value::Null,
        200,
    )
    .await;
    request(
        &router,
        "actions/create-repo-variable",
        "POST",
        base,
        Some(&outsider),
        json!({"name":"NO_ADMIN","value":"bad"}),
        403,
    )
    .await;
    let client = Client::login(&router, f.pool(), alice).await;
    let page = client.page_ok("/alice/project/settings?tab=secrets").await;
    assert!(page.contains("NEW_REGION"));
    let reply = client
        .form_reply(
            "/alice/project/_forms/variable-save",
            &[
                ("repository_id", &repo.id.to_string()),
                ("name", "FROM_FORM"),
                ("value", "native form variable"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text());
    assert!(db::variables::for_run(f.pool(), repo.id)
        .await
        .unwrap()
        .contains(&("FROM_FORM".into(), "native form variable".into())));
    let reply = client
        .form_reply(
            "/alice/project/_forms/variable-delete",
            &[
                ("repository_id", &repo.id.to_string()),
                ("name", "FROM_FORM"),
                ("confirm", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    request(
        &router,
        "actions/delete-repo-variable",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&writer),
        Value::Null,
        204,
    )
    .await;
    request(
        &router,
        "actions/delete-repo-variable",
        "DELETE",
        &format!("{base}/SECOND"),
        Some(&writer),
        Value::Null,
        404,
    )
    .await;
    f.finish().await;
}

pub(super) async fn gh_empty_value(
    router: &axum::Router,
    write_base: &str,
    read_base: &str,
    reader: &str,
    writer: &str,
    visibility: Option<&str>,
) {
    let area = if visibility.is_some() {
        "org"
    } else if read_base.contains("/environments/") {
        "environment"
    } else {
        "repo"
    };
    let create_op = format!("actions/create-{area}-variable");
    let get_op = format!("actions/get-{area}-variable");
    let update_op = format!("actions/update-{area}-variable");
    let delete_op = format!("actions/delete-{area}-variable");
    let mut create = json!({"name":"GH_EMPTY"});
    let mut update = json!({});
    if let Some(visibility) = visibility {
        create["visibility"] = json!(visibility);
        update["visibility"] = json!(visibility);
    }
    // These are gh's omitempty payloads, including its create-conflict-patch upsert.
    request(
        router,
        &create_op,
        "POST",
        write_base,
        Some(writer),
        create.clone(),
        201,
    )
    .await;
    let path = format!("{write_base}/GH_EMPTY");
    let read = format!("{read_base}/GH_EMPTY");
    let (_, empty) = request(
        router,
        &get_op,
        "GET",
        &read,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty["name"], "GH_EMPTY");
    assert_eq!(empty["value"], "");
    for field in ["created_at", "updated_at"] {
        chrono::DateTime::parse_from_rfc3339(empty[field].as_str().unwrap()).unwrap();
    }
    let mut filled = update.clone();
    filled["value"] = json!("before clearing");
    request(
        router,
        &update_op,
        "PATCH",
        &path,
        Some(writer),
        filled,
        204,
    )
    .await;
    request(
        router,
        &create_op,
        "POST",
        write_base,
        Some(writer),
        create,
        409,
    )
    .await;
    request(
        router,
        &update_op,
        "PATCH",
        &path,
        Some(writer),
        update,
        204,
    )
    .await;
    let (_, cleared) = request(
        router,
        &get_op,
        "GET",
        &read,
        Some(reader),
        Value::Null,
        200,
    )
    .await;
    // An empty PATCH clears the value for gh's upsert of an empty variable. One that sets the
    // visibility leaves an omitted value as it is, as GitHub does.
    let kept = if visibility.is_some() {
        "before clearing"
    } else {
        ""
    };
    assert_eq!(cleared["value"], kept);
    assert_eq!(cleared["created_at"], empty["created_at"]);
    if let Some(visibility) = visibility {
        assert_eq!(cleared["visibility"], visibility);
        assert!(cleared["selected_repositories_url"]
            .as_str()
            .unwrap()
            .ends_with("/GH_EMPTY/repositories"));
    }
    request(
        router,
        &delete_op,
        "DELETE",
        &read,
        Some(writer),
        Value::Null,
        204,
    )
    .await;
}

#[tokio::test]
async fn repository_variables_sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn repository_variables_postgres() {
    contract(true).await;
}
