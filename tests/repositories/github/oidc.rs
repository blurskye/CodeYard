use super::*;

async fn enterprise_oidc_is_not_supported(pg: bool) {
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let base = "/api/v3/enterprises/example/actions/oidc/customization/properties/repo";
    for (method, path, input) in [
        ("GET", base.to_owned(), Value::Null),
        (
            "POST",
            base.to_owned(),
            json!({"custom_property_name":"team"}),
        ),
        ("DELETE", format!("{base}/team"), Value::Null),
    ] {
        let (status, _, body) = request(&router, method, &path, None, input).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {body}");
        assert_eq!(body["message"], "Not Found");
        assert!(body["documentation_url"]
            .as_str()
            .unwrap()
            .starts_with("https://docs.github.com/rest/actions/oidc#"));
    }
}

#[tokio::test]
async fn sqlite_enterprise_oidc_is_not_supported() {
    enterprise_oidc_is_not_supported(false).await;
}

#[tokio::test]
async fn postgres_enterprise_oidc_is_not_supported() {
    enterprise_oidc_is_not_supported(true).await;
}
