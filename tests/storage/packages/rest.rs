use super::*;
use crate::support::github_spec::assert_github_schema;
use std::io::Write;

async fn rest(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    expected: u16,
) -> (HeaderMap, Value) {
    let (status, headers, bytes) = request(
        router,
        method,
        path,
        token,
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(
        status.as_u16(),
        expected,
        "{method} {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body = if expected == 204 || method == "HEAD" {
        assert!(bytes.is_empty());
        Value::Null
    } else {
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_github_schema(operation, expected, &body);
        body
    };
    (headers, body)
}

fn tarball(owner: &str, package: &str, version: &str) -> Vec<u8> {
    let mut tar = vec![0u8; 1024];
    let data = json!({"name":format!("@{owner}/{package}"),"version":version}).to_string();
    let path = b"package/package.json";
    tar[..path.len()].copy_from_slice(path);
    tar[100..108].copy_from_slice(b"0000644\0");
    tar[108..116].copy_from_slice(b"0000000\0");
    tar[116..124].copy_from_slice(b"0000000\0");
    tar[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    tar[136..148].copy_from_slice(b"00000000000\0");
    tar[148..156].fill(b' ');
    tar[156] = b'0';
    tar[257..263].copy_from_slice(b"ustar\0");
    tar[263..265].copy_from_slice(b"00");
    let checksum: usize = tar[..512].iter().map(|b| *b as usize).sum();
    tar[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    tar[512..512 + data.len()].copy_from_slice(data.as_bytes());
    tar.resize(2048, 0);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar).unwrap();
    gzip.finish().unwrap()
}

pub(super) async fn publish(
    router: &Router,
    writer: &str,
    owner: &str,
    package: &str,
    version: &str,
) -> Vec<u8> {
    let bytes = tarball(owner, package, version);
    let name = format!("@{owner}/{package}");
    let input = json!({"_id":name,"name":name,"versions":{version:{"name":name,"version":version,"description":"A package"}},"dist-tags":{"latest":version},"_attachments":{format!("{package}-{version}.tgz"):{"content_type":"application/octet-stream","data":STANDARD.encode(&bytes),"length":bytes.len()}}});
    let (status, _, body) = request(
        router,
        "PUT",
        &format!("/api/packages/{owner}/npm/@{owner}%2f{package}"),
        Some(writer),
        "application/json",
        input.to_string().into_bytes(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    bytes
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("package-owner").await;
    let bob = f.actor("package-outsider").await;
    f.execute("INSERT INTO organizations(name) VALUES('package-org')", &[])
        .await;
    let org = f
        .scalar("SELECT id FROM organizations WHERE name='package-org'", &[])
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, alice],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = format!("Bearer {}", token(&f, &router, alice, "write").await);
    let reader = format!("Bearer {}", token(&f, &router, alice, "read").await);
    let outsider = format!("Bearer {}", token(&f, &router, bob, "write").await);
    for (owner, base, operations) in [
        (
            "package-owner",
            "/api/v3/user/packages",
            [
                "packages/list-packages-for-authenticated-user",
                "packages/get-package-for-authenticated-user",
                "packages/get-all-package-versions-for-package-owned-by-authenticated-user",
                "packages/get-package-version-for-authenticated-user",
                "packages/delete-package-version-for-authenticated-user",
                "packages/restore-package-version-for-authenticated-user",
                "packages/delete-package-for-authenticated-user",
                "packages/restore-package-for-authenticated-user",
            ],
        ),
        (
            "package-owner",
            "/api/v3/users/package-owner/packages",
            [
                "packages/list-packages-for-user",
                "packages/get-package-for-user",
                "packages/get-all-package-versions-for-package-owned-by-user",
                "packages/get-package-version-for-user",
                "packages/delete-package-version-for-user",
                "packages/restore-package-version-for-user",
                "packages/delete-package-for-user",
                "packages/restore-package-for-user",
            ],
        ),
        (
            "package-org",
            "/api/v3/orgs/package-org/packages",
            [
                "packages/list-packages-for-organization",
                "packages/get-package-for-organization",
                "packages/get-all-package-versions-for-package-owned-by-org",
                "packages/get-package-version-for-organization",
                "packages/delete-package-version-for-org",
                "packages/restore-package-version-for-org",
                "packages/delete-package-for-org",
                "packages/restore-package-for-org",
            ],
        ),
    ] {
        // The personal /user and /users routes operate on the very same published versions.
        if !base.contains("/users/") {
            publish(&router, &writer, owner, "library", "1.0.0").await;
            publish(&router, &writer, owner, "library", "2.0.0").await;
            publish(&router, &writer, owner, "other", "1.0.0").await;
        }
        let (headers, body) = rest(
            &router,
            operations[0],
            "GET",
            &format!("{base}?package_type=npm&per_page=1"),
            Some(&reader),
            200,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
        let package = format!("{base}/npm/library");
        let (_, body) = rest(&router, operations[1], "GET", &package, Some(&reader), 200).await;
        assert_eq!(body["version_count"], 2);
        assert_eq!(body["owner"]["login"], owner);
        let (_, body) = rest(
            &router,
            operations[2],
            "GET",
            &format!("{package}/versions"),
            Some(&reader),
            200,
        )
        .await;
        let id = body[0]["id"].as_i64().unwrap();
        let version = format!("{package}/versions/{id}");
        rest(&router, operations[3], "GET", &version, Some(&reader), 200).await;
        rest(
            &router,
            operations[3],
            "GET",
            &format!("{package}/versions/invalid"),
            Some(&reader),
            404,
        )
        .await;
        rest(
            &router,
            operations[4],
            "DELETE",
            &version,
            Some(&reader),
            403,
        )
        .await;
        rest(
            &router,
            operations[4],
            "DELETE",
            &version,
            Some(&writer),
            204,
        )
        .await;
        rest(&router, operations[3], "GET", &version, Some(&reader), 404).await;
        let (_, body) = rest(
            &router,
            operations[2],
            "GET",
            &format!("{package}/versions?state=deleted"),
            Some(&writer),
            200,
        )
        .await;
        assert_eq!(body[0]["id"], id);
        assert!(body[0].get("deleted_at").is_some());
        rest(
            &router,
            operations[5],
            "POST",
            &format!("{version}/restore"),
            Some(&writer),
            204,
        )
        .await;
        rest(&router, operations[3], "GET", &version, Some(&reader), 200).await;
        rest(
            &router,
            operations[6],
            "DELETE",
            &package,
            Some(&reader),
            403,
        )
        .await;
        rest(
            &router,
            operations[6],
            "DELETE",
            &package,
            Some(&writer),
            204,
        )
        .await;
        rest(&router, operations[1], "GET", &package, Some(&reader), 404).await;
        rest(
            &router,
            operations[7],
            "POST",
            &format!("{package}/restore"),
            Some(&writer),
            204,
        )
        .await;
        rest(&router, operations[1], "GET", &package, Some(&reader), 200).await;
        let (_, body) = rest(
            &router,
            operations[1],
            "GET",
            &format!("{base}/invalid/library"),
            Some(&reader),
            422,
        )
        .await;
        assert!(body["errors"].is_array());
        if !base.ends_with("/user/packages") {
            rest(
                &router,
                operations[1],
                "GET",
                &package,
                Some(&outsider),
                404,
            )
            .await;
            rest(
                &router,
                operations[4],
                "DELETE",
                &version,
                Some(&outsider),
                404,
            )
            .await;
            rest(&router, operations[1], "GET", &package, None, 404).await;
            let (_, body) = rest(
                &router,
                operations[0],
                "GET",
                &format!("{base}?package_type=npm"),
                Some(&outsider),
                200,
            )
            .await;
            assert_eq!(body, json!([]));
        }
    }
    rest(
        &router,
        "packages/list-packages-for-authenticated-user",
        "GET",
        "/api/v3/user/packages?package_type=npm",
        None,
        401,
    )
    .await;
    // npm install obtains a packument, verifies its integrity, and downloads the exact tarball.
    let path = "/api/packages/package-owner/npm/@package-owner%2flibrary";
    let (status, _, bytes) = request(
        &router,
        "GET",
        path,
        Some(&reader),
        "application/json",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let packument: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(packument["dist-tags"]["latest"], "2.0.0");
    let dist = &packument["versions"]["2.0.0"]["dist"];
    let url = url::Url::parse(dist["tarball"].as_str().unwrap()).unwrap();
    let (status, _, bytes) = request(
        &router,
        "GET",
        url.path(),
        Some(&reader),
        "application/octet-stream",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, tarball("package-owner", "library", "2.0.0"));
    use sha2::{Digest, Sha512};
    assert_eq!(
        dist["integrity"],
        format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes)))
    );
    assert_eq!(dist["shasum"], hex::encode(sha1::Sha1::digest(&bytes)));
    let tarball = tarball("package-owner", "library", "3.0.0");
    let input = json!({"name":"@package-owner/library","versions":{"3.0.0":{"name":"@package-owner/library","version":"3.0.0"}},"dist-tags":{"latest":"3.0.0"},"_attachments":{"library.tgz":{"data":STANDARD.encode(&tarball),"length":tarball.len()}}}).to_string().into_bytes();
    for (token, expected) in [
        (Some(reader.as_str()), StatusCode::FORBIDDEN),
        (Some(outsider.as_str()), StatusCode::NOT_FOUND),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        let (status, _, _) = request(
            &router,
            "PUT",
            path,
            token,
            "application/json",
            input.clone(),
            None,
        )
        .await;
        assert_eq!(status, expected);
    }
    let (status, _, _) = request(
        &router,
        "PUT",
        path,
        Some(&writer),
        "application/json",
        input.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = request(
        &router,
        "PUT",
        path,
        Some(&writer),
        "application/json",
        input,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    f.finish().await;
}

#[tokio::test]
async fn packages_rest_and_npm_sqlite() {
    contract(false).await;
}
#[tokio::test]
async fn packages_rest_and_npm_postgres() {
    contract(true).await;
}
