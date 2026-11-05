use crate::support::fake_s3::FakeS3;
use crate::support::native_database;

use codeyard_storage::s3::create_client;
use codeyard_storage::s3::S3Store;
use reqwest::StatusCode;
use std::time::Duration;

#[tokio::test]
async fn fake_s3_is_only_started_for_cluster_databases() {
    let sqlite = native_database::NativeDatabase::new().await;
    assert!(FakeS3::for_database(&sqlite.pool).await.is_none());
    sqlite.close().await;
    let mut postgres = native_database::PostgresDatabase::new().await;
    assert!(FakeS3::for_database(&postgres.pool).await.is_none());
    crate::support::native_database::enable_cluster(&postgres.pool)
        .await
        .unwrap();
    let s3 = FakeS3::for_database(&postgres.pool).await.unwrap();
    let environment = s3.environment();
    assert_eq!(environment[0], ("S3_ENDPOINT", s3.config.endpoint.as_str()));
    assert_eq!(environment[4], ("S3_BUCKET", "codeyard-test"));
    drop(s3);
    postgres.finish().await;
}

#[tokio::test]
async fn fake_s3_keeps_bucket_and_cors_but_refuses_objects() {
    let s3 = FakeS3::start().await;
    s3.config
        .validate_browser_cookie_scope(Some("http://127.0.0.1:3000"), None)
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let bucket = format!("{}/{}", s3.config.endpoint, s3.config.bucket);
    assert_eq!(
        client.head(&bucket).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let missing = client.get(format!("{bucket}?cors")).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(missing.text().await.unwrap().contains("NoSuchBucket"));
    let store = S3Store::open(s3.config.clone()).await.unwrap();
    store.ensure_bucket().await.unwrap();
    assert_eq!(
        client.head(&bucket).send().await.unwrap().status(),
        StatusCode::OK
    );
    let cors = format!("{bucket}?cors");
    let xml = "<CORSConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><CORSRule><AllowedOrigin>http://127.0.0.1:3000</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>";
    assert_eq!(
        client.put(&cors).body(xml).send().await.unwrap().status(),
        StatusCode::OK
    );
    store.ensure_bucket().await.unwrap();
    assert_eq!(
        client
            .get(&cors)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        xml
    );
    let sdk = create_client(&s3.config).await.unwrap();
    let config = sdk
        .get_bucket_cors()
        .bucket(&s3.config.bucket)
        .send()
        .await
        .unwrap();
    assert_eq!(config.cors_rules().len(), 1);
    assert_eq!(config.cors_rules()[0].allowed_methods(), &["GET"]);
    for method in [
        reqwest::Method::HEAD,
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        assert_eq!(
            client
                .request(method, format!("{bucket}/object"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_IMPLEMENTED
        );
    }
}
