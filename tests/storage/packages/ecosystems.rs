//! Client-shaped Maven/Gradle, NuGet and RubyGems conversations on both databases.
use super::*;
use codeyard::backend::{ci::tokens::Permissions, db};
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
};

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    bytes: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}
async fn expect(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    bytes: Vec<u8>,
    status: u16,
) -> (HeaderMap, Vec<u8>) {
    let (actual, headers, body) = send(router, method, path, headers, bytes).await;
    assert_eq!(
        actual.as_u16(),
        status,
        "{method} {path}: {}",
        String::from_utf8_lossy(&body)
    );
    (headers, body)
}
fn basic(token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("publisher:{token}")))
}
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(bytes).unwrap();
    gzip.finish().unwrap()
}
fn gem(name: &str, version: &str, platform: &str) -> Vec<u8> {
    let metadata=format!("--- !ruby/object:Gem::Specification\nname: {name}\nversion: !ruby/object:Gem::Version\n  version: '{version}'\nplatform: {platform}\ndependencies:\n- !ruby/object:Gem::Dependency\n  name: rack\n  type: :runtime\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - '~>'\n      - !ruby/object:Gem::Version\n        version: '3.0'\nrequired_ruby_version: &ruby_requirement !ruby/object:Gem::Requirement\n  requirements:\n  - - '>='\n    - !ruby/object:Gem::Version\n      version: '2.7'\nrequired_rubygems_version: *ruby_requirement\n");
    let metadata = if name == "rack" {
        format!("--- !ruby/object:Gem::Specification\nname: rack\nversion: !ruby/object:Gem::Version\n  version: '{version}'\nplatform: ruby\n")
    } else {
        metadata
    };
    let metadata = format!("{metadata}rubygems_version: '3.6.0'\nspecification_version: 4\nsummary: A test gem\nauthors:\n- Publisher\nfiles:\n- lib/demo.rb\nrequire_paths:\n- lib\nlicenses:\n- MIT\n");
    let mut data = tar::Builder::new(Vec::new());
    let source = b"module Demo; end\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(source.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    data.append_data(&mut header, "lib/demo.rb", &source[..])
        .unwrap();
    let data = gzip(&data.into_inner().unwrap());
    let metadata = gzip(metadata.as_bytes());
    let mut archive = tar::Builder::new(Vec::new());
    for (name, bytes) in [("metadata.gz", metadata), ("data.tar.gz", data)] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, name, &bytes[..]).unwrap();
    }
    archive.into_inner().unwrap()
}
fn nupkg(name: &str, version: &str) -> Vec<u8> {
    let dependencies = if name == "Base.Library" {
        ""
    } else {
        "<group targetFramework=\"net8.0\"><dependency id=\"Base.Library\" version=\"[1.0.0,2.0.0)\" /></group>"
    };
    let package_types = if name == "Symbol.Package" {
        "<packageTypes><packageType name=\"SymbolsPackage\" /></packageTypes>"
    } else {
        ""
    };
    let xml=format!("<?xml version=\"1.0\"?><package xmlns=\"http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd\"><metadata><id>{name}</id><version>{version}</version><authors>Example,Publisher</authors><description>Example &amp; library</description><dependencies>{dependencies}</dependencies>{package_types}</metadata></package>");
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(format!("{name}.nuspec"), options).unwrap();
    zip.write_all(xml.as_bytes()).unwrap();
    zip.start_file("lib/net8.0/_._", options).unwrap();
    zip.write_all(b"").unwrap();
    zip.finish().unwrap().into_inner()
}
fn multipart(bytes: &[u8]) -> Vec<u8> {
    let mut body=b"--nuget-boundary\r\nContent-Type: application/octet-stream\r\nContent-Disposition: form-data; name=package; filename=package.nupkg\r\n\r\n".to_vec();
    body.extend(bytes);
    body.extend(b"\r\n--nuget-boundary--\r\n");
    body
}
async fn github(
    router: &Router,
    operation: &str,
    method: &str,
    path: &str,
    auth: &str,
    status: u16,
) -> Value {
    let (_, bytes) = expect(
        router,
        method,
        path,
        &[("authorization", auth)],
        vec![],
        status,
    )
    .await;
    if status == 204 {
        assert!(bytes.is_empty());
        Value::Null
    } else {
        let json = serde_json::from_slice(&bytes).unwrap();
        crate::support::github_spec::assert_github_schema(operation, status, &json);
        json
    }
}
#[allow(clippy::too_many_arguments)]
async fn lifecycle(
    router: &Router,
    f: &Database,
    writer: &str,
    reader: &str,
    owner: i64,
    kind: &str,
    name: &str,
    download: &str,
) {
    let encoded = codeyard::webui::encode_component(name);
    let base = format!("/api/v3/users/ecosystem-owner/packages/{kind}/{encoded}");
    let package = github(
        router,
        "packages/get-package-for-user",
        "GET",
        &base,
        reader,
        200,
    )
    .await;
    assert_eq!(package["package_type"], kind);
    let versions = github(
        router,
        "packages/get-all-package-versions-for-package-owned-by-user",
        "GET",
        &format!("{base}/versions"),
        reader,
        200,
    )
    .await;
    let id = versions[0]["id"].as_i64().unwrap();
    let version = format!("{base}/versions/{id}");
    github(
        router,
        "packages/get-package-version-for-user",
        "GET",
        &version,
        reader,
        200,
    )
    .await;
    let cached_info = if kind == "rubygems" {
        Some(
            expect(
                router,
                "GET",
                "/api/packages/ecosystem-owner/rubygems/info/demo-gem",
                &[("authorization", reader)],
                vec![],
                200,
            )
            .await,
        )
    } else {
        None
    };
    github(
        router,
        "packages/delete-package-version-for-user",
        "DELETE",
        &version,
        writer,
        204,
    )
    .await;
    expect(
        router,
        "GET",
        download,
        &[("authorization", reader)],
        vec![],
        404,
    )
    .await;
    if let Some((headers, body)) = cached_info {
        let range = format!("bytes={}-", body.len() - 1);
        let (_, fresh) = expect(
            router,
            "GET",
            "/api/packages/ecosystem-owner/rubygems/info/demo-gem",
            &[
                ("authorization", reader),
                ("range", &range),
                ("if-none-match", headers["etag"].to_str().unwrap()),
            ],
            vec![],
            200,
        )
        .await;
        let removed = versions[0]["name"].as_str().unwrap();
        assert!(!String::from_utf8(fresh)
            .unwrap()
            .contains(&format!("{removed} ")));
    }
    github(
        router,
        "packages/restore-package-version-for-user",
        "POST",
        &format!("{version}/restore"),
        writer,
        204,
    )
    .await;
    expect(
        router,
        "GET",
        download,
        &[("authorization", reader)],
        vec![],
        200,
    )
    .await;
    github(
        router,
        "packages/delete-package-for-user",
        "DELETE",
        &base,
        writer,
        204,
    )
    .await;
    expect(
        router,
        "GET",
        download,
        &[("authorization", reader)],
        vec![],
        404,
    )
    .await;
    github(
        router,
        "packages/restore-package-for-user",
        "POST",
        &format!("{base}/restore"),
        writer,
        204,
    )
    .await;
    let user = Client::login(router, f.pool(), owner).await;
    let path = format!("/users/ecosystem-owner/packages/{kind}/{encoded}");
    let html = user.page_ok(&path).await;
    assert!(
        html.contains(match kind {
            "maven" => "implementation",
            "nuget" => "dotnet add package",
            _ => "bundle install",
        }),
        "{html}"
    );
    let package_id = package["id"].as_i64().unwrap().to_string();
    let settings = format!("{path}/settings");
    let result = user
        .form_reply(
            &settings,
            &[
                ("action", "settings"),
                ("visibility", "public"),
                ("repository", "repository"),
                ("package_id", &package_id),
            ],
        )
        .await;
    assert_eq!(result.status, StatusCode::SEE_OTHER, "{}", result.text());
    let html = Client::anonymous(router)
        .page_ok(&format!("/explore/packages?type={kind}"))
        .await;
    assert!(html.contains(name), "{html}");
    expect(router, "GET", download, &[], vec![], 200).await;
    let list = github(
        router,
        "packages/list-packages-for-user",
        "GET",
        &format!("/api/v3/users/ecosystem-owner/packages?package_type={kind}"),
        reader,
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

async fn ecosystems(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("ecosystem-owner").await;
    let bob = f.actor("ecosystem-outsider").await;
    f.execute("INSERT INTO repositories(owner_user_id,name,disk_path,is_private) VALUES($1,'repository','ecosystem-repository.git',false),($1,'other','ecosystem-other.git',true)",&[alice]).await;
    let repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_user_id=$1 AND name='repository'",
            &[alice],
        )
        .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let pat = token(&f, &router, alice, "write").await;
    let read_pat = token(&f, &router, alice, "read").await;
    let writer = format!("Bearer {pat}");
    let reader = format!("Bearer {read_pat}");
    let basic_auth = basic(&pat);
    let read_basic = basic(&read_pat);
    let outsider = format!("Bearer {}", token(&f, &router, bob, "write").await);
    let write_headers = [
        ("authorization", basic_auth.as_str()),
        ("user-agent", "Apache-Maven/3.9.9"),
    ];
    let read_headers = [("authorization", read_basic.as_str())];
    // Maven/Gradle deploy: POM, main/classified artifacts, metadata and checksum PUTs.
    let pom=b"<project><modelVersion>4.0.0</modelVersion><groupId>com.example</groupId><artifactId>demo</artifactId><version>1.0.0</version></project>";
    let base = "/ecosystem-owner/repository/com/example/demo";
    let jar = format!("{base}/1.0.0/demo-1.0.0.jar");
    expect(
        &router,
        "PUT",
        &format!("{base}/1.0.0/demo-1.0.0.pom"),
        &write_headers,
        pom.to_vec(),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &jar,
        &write_headers,
        b"release jar".to_vec(),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &format!("{base}/1.0.0/demo-1.0.0-sources.jar"),
        &write_headers,
        b"sources jar".to_vec(),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &format!("{base}/1.0.0/demo-1.0.0.module"),
        &write_headers,
        b"{\"formatVersion\":\"1.1\"}".to_vec(),
        201,
    )
    .await;
    let metadata=b"<metadata><groupId>com.example</groupId><artifactId>demo</artifactId><versioning><latest>1.0.0</latest><release>1.0.0</release><versions><version>1.0.0</version></versions></versioning></metadata>";
    expect(
        &router,
        "PUT",
        &format!("{base}/maven-metadata.xml"),
        &write_headers,
        metadata.to_vec(),
        201,
    )
    .await;
    let (challenge, _) = expect(&router, "GET", &jar, &[], vec![], 401).await;
    assert!(challenge["www-authenticate"]
        .to_str()
        .unwrap()
        .starts_with("Basic "));
    expect(
        &router,
        "GET",
        "/ecosystem-owner/repository/com/example/missing/1.0/missing-1.0.jar",
        &[],
        vec![],
        401,
    )
    .await;
    for (algorithm, expected) in [
        ("md5", hex::encode(md5::Md5::digest(b"release jar"))),
        ("sha256", hex::encode(sha2::Sha256::digest(b"release jar"))),
        ("sha512", hex::encode(Sha512::digest(b"release jar"))),
    ] {
        expect(
            &router,
            "PUT",
            &format!("{jar}.{algorithm}"),
            &write_headers,
            expected.as_bytes().to_vec(),
            201,
        )
        .await;
        assert_eq!(
            expect(
                &router,
                "GET",
                &format!("{jar}.{algorithm}"),
                &read_headers,
                vec![],
                200
            )
            .await
            .1,
            expected.as_bytes()
        );
    }
    let checksum = hex::encode(sha1::Sha1::digest(b"release jar"));
    expect(
        &router,
        "PUT",
        &format!("{jar}.sha1"),
        &write_headers,
        checksum.as_bytes().to_vec(),
        201,
    )
    .await;
    assert_eq!(
        expect(
            &router,
            "GET",
            &format!("{jar}.sha1"),
            &read_headers,
            vec![],
            200
        )
        .await
        .1,
        checksum.as_bytes()
    );
    expect(
        &router,
        "PUT",
        &format!("{jar}.sha1"),
        &write_headers,
        b"wrong".to_vec(),
        400,
    )
    .await;
    expect(
        &router,
        "PUT",
        &jar,
        &write_headers,
        b"overwrite release".to_vec(),
        409,
    )
    .await;
    expect(&router, "PUT", &jar, &read_headers, b"no".to_vec(), 403).await;
    expect(
        &router,
        "GET",
        &jar,
        &[("authorization", &outsider)],
        vec![],
        404,
    )
    .await;
    let (headers, body) = expect(&router, "HEAD", &jar, &read_headers, vec![], 200).await;
    assert!(body.is_empty());
    assert_eq!(headers["content-length"], "11");
    // Maven plugin-prefix group metadata has no artifactId and may omit groupId.
    let group_meta = "/ecosystem-owner/repository/com/example/maven-metadata.xml";
    let catalog = b"<metadata><plugins><plugin><name>Demo</name><prefix>demo</prefix><artifactId>demo</artifactId></plugin></plugins></metadata>";
    expect(
        &router,
        "PUT",
        group_meta,
        &write_headers,
        catalog.to_vec(),
        201,
    )
    .await;
    assert_eq!(
        expect(&router, "GET", group_meta, &read_headers, vec![], 200)
            .await
            .1,
        catalog
    );
    expect(
        &router,
        "PUT",
        "/ecosystem-owner/repository/com/example/tool/1.0/tool-1.0.jar",
        &write_headers,
        b"plugin".to_vec(),
        201,
    )
    .await;
    let second_catalog = b"<metadata><plugins><plugin><prefix>tool</prefix><artifactId>tool</artifactId></plugin><plugin><prefix>demo</prefix><artifactId>demo</artifactId></plugin></plugins></metadata>";
    expect(
        &router,
        "PUT",
        group_meta,
        &write_headers,
        second_catalog.to_vec(),
        201,
    )
    .await;
    assert_eq!(
        expect(&router, "GET", group_meta, &read_headers, vec![], 200)
            .await
            .1,
        second_catalog
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM package_files WHERE path='com/example/maven-metadata.xml'",
            &[]
        )
        .await,
        1
    );
    github(
        &router,
        "packages/delete-package-for-user",
        "DELETE",
        "/api/v3/users/ecosystem-owner/packages/maven/com.example%3Atool",
        &writer,
        204,
    )
    .await;
    expect(
        &router,
        "PUT",
        group_meta,
        &write_headers,
        catalog.to_vec(),
        201,
    )
    .await;
    let snapshot = format!("{base}/1.1-SNAPSHOT/demo-1.1-20261023.010203-1.jar");
    expect(
        &router,
        "PUT",
        &snapshot,
        &write_headers,
        b"snapshot old".to_vec(),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &snapshot,
        &write_headers,
        b"snapshot new".to_vec(),
        201,
    )
    .await;
    let snapshot_meta=b"<metadata><groupId>com.example</groupId><artifactId>demo</artifactId><version>1.1-SNAPSHOT</version><versioning><snapshot><timestamp>20261023.010203</timestamp><buildNumber>1</buildNumber></snapshot><snapshotVersions><snapshotVersion><extension>jar</extension><value>1.1-20261023.010203-1</value><updated>20261023010203</updated></snapshotVersion></snapshotVersions></versioning></metadata>";
    expect(
        &router,
        "PUT",
        &format!("{base}/1.1-SNAPSHOT/maven-metadata.xml"),
        &write_headers,
        snapshot_meta.to_vec(),
        201,
    )
    .await;
    assert_eq!(
        expect(&router, "GET", &snapshot, &read_headers, vec![], 200)
            .await
            .1,
        b"snapshot new"
    );
    assert_eq!(
        expect(
            &router,
            "GET",
            &format!("{base}/1.1-SNAPSHOT/maven-metadata.xml"),
            &read_headers,
            vec![],
            200
        )
        .await
        .1,
        snapshot_meta
    );

    // NuGet discovers every resource from the index, not hard-coded nuget.org URLs.
    let nuget = "/api/packages/ecosystem-owner/nuget";
    let index = serde_json::from_slice::<Value>(
        &expect(
            &router,
            "GET",
            &format!("{nuget}/index.json"),
            &[],
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(index["version"], "3.0.0");
    let publish = index["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["@type"] == "PackagePublish/2.0.0")
        .unwrap()["@id"]
        .as_str()
        .unwrap();
    let publish = url::Url::parse(publish).unwrap().path().to_owned();
    expect(
        &router,
        "PUT",
        &publish,
        &[
            ("x-nuget-apikey", pat.as_str()),
            ("content-type", "application/octet-stream"),
        ],
        nupkg("Symbol.Package", "1.0.0"),
        400,
    )
    .await;
    let bytes = nupkg("Example.Library", "1.0.0-RC.1+build");
    // A configured NuGet source can send HTTP Basic credentials alongside its API key.
    let push_headers = [
        ("authorization", read_basic.as_str()),
        ("x-nuget-apikey", pat.as_str()),
        (
            "content-type",
            "multipart/form-data; boundary=nuget-boundary",
        ),
        ("user-agent", "NuGet Command Line/6.12.1"),
    ];
    expect(
        &router,
        "PUT",
        &publish,
        &push_headers,
        multipart(&bytes),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &publish,
        &push_headers,
        multipart(&bytes),
        409,
    )
    .await;
    let stable = nupkg("Example.Library", "1.0.0");
    expect(
        &router,
        "PUT",
        &publish,
        &push_headers,
        multipart(&stable),
        201,
    )
    .await;
    let dependency = nupkg("Base.Library", "1.0.0");
    expect(
        &router,
        "PUT",
        &publish,
        &push_headers,
        multipart(&dependency),
        201,
    )
    .await;
    for path in [
        "flatcontainer/base.library/index.json",
        "registration/base.library/index.json",
        "flatcontainer/base.library/1.0.0/base.library.1.0.0.nupkg",
    ] {
        expect(
            &router,
            "GET",
            &format!("{nuget}/{path}"),
            &read_headers,
            vec![],
            200,
        )
        .await;
    }
    expect(
        &router,
        "PUT",
        &publish,
        &[
            ("authorization", basic_auth.as_str()),
            ("x-nuget-apikey", read_pat.as_str()),
            ("content-type", "application/octet-stream"),
        ],
        nupkg("Read.Denied", "1.0.0"),
        403,
    )
    .await;
    let flat = format!("{nuget}/flatcontainer/example.library");
    let json = serde_json::from_slice::<Value>(
        &expect(
            &router,
            "GET",
            &format!("{flat}/index.json"),
            &read_headers,
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(json["versions"], json!(["1.0.0-rc.1", "1.0.0"]));
    let download = format!("{flat}/1.0.0/example.library.1.0.0.nupkg");
    expect(&router, "GET", &download, &[], vec![], 401).await;
    assert_eq!(
        expect(&router, "GET", &download, &read_headers, vec![], 200)
            .await
            .1,
        stable
    );
    let checksum = expect(
        &router,
        "GET",
        &format!("{download}.sha512"),
        &read_headers,
        vec![],
        200,
    )
    .await
    .1;
    assert_eq!(
        checksum,
        STANDARD.encode(Sha512::digest(&stable)).as_bytes()
    );
    let xml = expect(
        &router,
        "GET",
        &format!("{flat}/1.0.0/example.library.nuspec"),
        &read_headers,
        vec![],
        200,
    )
    .await
    .1;
    assert!(String::from_utf8(xml).unwrap().contains("Base.Library"));
    let registration = serde_json::from_slice::<Value>(
        &expect(
            &router,
            "GET",
            &format!("{nuget}/registration/example.library/index.json"),
            &read_headers,
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(
        registration["items"][0]["items"][1]["catalogEntry"]["dependencyGroups"][0]["dependencies"]
            [0]["range"],
        "[1.0.0,2.0.0)"
    );
    let leaf = serde_json::from_slice::<Value>(
        &expect(
            &router,
            "GET",
            &format!("{nuget}/registration/example.library/1.0.0.json"),
            &read_headers,
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    let catalog = url::Url::parse(leaf["catalogEntry"].as_str().unwrap()).unwrap();
    let catalog = serde_json::from_slice::<Value>(
        &expect(&router, "GET", catalog.path(), &read_headers, vec![], 200)
            .await
            .1,
    )
    .unwrap();
    assert_eq!(catalog["id"], "Example.Library");
    let search = serde_json::from_slice::<Value>(
        &expect(
            &router,
            "GET",
            &format!(
                "{nuget}/query?q=example.library&skip=0&take=1&prerelease=false&semVerLevel=2.0.0"
            ),
            &read_headers,
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(search["totalHits"], 1);
    assert_eq!(search["data"][0]["version"], "1.0.0");
    assert_eq!(search["data"][0]["totalDownloads"], 1);
    assert_eq!(search["data"][0]["versions"][0]["downloads"], 1);
    assert_eq!(
        search["data"][0]["authors"],
        json!(["Example", "Publisher"])
    );
    expect(
        &router,
        "GET",
        &download,
        &[("authorization", &outsider)],
        vec![],
        404,
    )
    .await;

    // gem push sends the raw API key in Authorization; Bundler uses Basic credentials.
    let ruby = "/api/packages/ecosystem-owner/rubygems";
    let gem_bytes = gem("demo-gem", "1.0.0", "ruby");
    let gem_headers = [
        ("authorization", pat.as_str()),
        ("content-type", "application/octet-stream"),
        ("user-agent", "RubyGems/3.6.0"),
    ];
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &gem_headers,
        gem_bytes.clone(),
        200,
    )
    .await;
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &gem_headers,
        gem("rack", "3.0.0", "ruby"),
        200,
    )
    .await;
    for path in ["info/rack", "gems/rack-3.0.0.gem"] {
        expect(
            &router,
            "GET",
            &format!("{ruby}/{path}"),
            &read_headers,
            vec![],
            200,
        )
        .await;
    }
    let platform_gem = gem("demo-gem", "1.0.0", "x86_64-linux");
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &gem_headers,
        platform_gem,
        200,
    )
    .await;
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &gem_headers,
        gem_bytes.clone(),
        409,
    )
    .await;
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &[("authorization", read_pat.as_str())],
        gem("denied-gem", "1.0.0", "ruby"),
        403,
    )
    .await;
    let info = format!("{ruby}/info/demo-gem");
    expect(
        &router,
        "GET",
        &format!("{ruby}/versions"),
        &[],
        vec![],
        401,
    )
    .await;
    expect(&router, "GET", &info, &[], vec![], 401).await;
    let (headers, body) = expect(&router, "GET", &info, &read_headers, vec![], 200).await;
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(text.contains("1.0.0 rack:~> 3.0|checksum:"), "{text}");
    assert!(text.contains("ruby:>= 2.7"));
    assert!(text.contains("1.0.0-x86_64-linux"));
    let digest = hex::encode(md5::Md5::digest(&body));
    let versions = String::from_utf8(
        expect(
            &router,
            "GET",
            &format!("{ruby}/versions"),
            &read_headers,
            vec![],
            200,
        )
        .await
        .1,
    )
    .unwrap();
    assert!(
        versions.contains(&format!("demo-gem 1.0.0,1.0.0-x86_64-linux {digest}")),
        "{versions}"
    );
    expect(
        &router,
        "GET",
        &info,
        &[
            ("authorization", &read_basic),
            ("if-none-match", headers["etag"].to_str().unwrap()),
        ],
        vec![],
        304,
    )
    .await;
    let (range, partial) = expect(
        &router,
        "GET",
        &info,
        &[("authorization", &read_basic), ("range", "bytes=4-")],
        vec![],
        206,
    )
    .await;
    assert_eq!(partial, &body[4..]);
    assert!(range["content-range"]
        .to_str()
        .unwrap()
        .starts_with("bytes 4-"));
    let gem_download = format!("{ruby}/gems/demo-gem-1.0.0-x86_64-linux.gem");
    expect(&router, "GET", &gem_download, &read_headers, vec![], 200).await;
    assert_eq!(
        expect(
            &router,
            "GET",
            &format!("{ruby}/gems/demo-gem-1.0.0.gem"),
            &read_headers,
            vec![],
            200
        )
        .await
        .1,
        gem_bytes
    );
    expect(
        &router,
        "GET",
        &gem_download,
        &[("authorization", &outsider)],
        vec![],
        404,
    )
    .await;

    // The same job token is accepted in each protocol's documented credential channel.
    let run_id = if pg {
        "'b00ca9c0-131e-4157-a7e5-12b353b069d8'"
    } else {
        "X'b00ca9c0131e4157a7e512b353b069d8'"
    };
    f.execute(&format!("INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,event,branch,head_sha,workflow_path,worker,status) VALUES({run_id},$1,$2,'local','push','main','0123456789012345678901234567890123456789','.github/workflows/publish.yml','ecosystem-worker','running')"),&[repo,alice]).await;
    let run = f
        .scalar("SELECT id FROM ci_runs WHERE repo_id=$1", &[repo])
        .await;
    let job = "ghs_ecosystems";
    let hash = tokens::token_hash(job);
    let permissions = serde_json::to_string(&Permissions(BTreeMap::from([
        ("packages".into(), 2),
        ("contents".into(), 2),
    ])))
    .unwrap();
    db::ci_tokens::issue(
        f.pool(),
        run,
        "ecosystem-worker",
        &[db::ci_tokens::NewJobToken {
            job: "publish",
            hash: &hash,
            permissions: &permissions,
            write: true,
        }],
    )
    .await
    .unwrap();
    expect(
        &router,
        "PUT",
        &format!("{base}/1.0.0/demo-1.0.0-javadoc.jar"),
        &[("authorization", &basic(job))],
        b"job javadoc".to_vec(),
        201,
    )
    .await;
    expect(
        &router,
        "PUT",
        &publish,
        &[("x-nuget-apikey", job)],
        nupkg("Job.Library", "1.0.0"),
        201,
    )
    .await;
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &[("authorization", job)],
        gem("job-gem", "1.0.0", "ruby"),
        200,
    )
    .await;
    expect(
        &router,
        "PUT",
        "/ecosystem-owner/other/com/example/no/1.0/no-1.0.jar",
        &[("authorization", &basic(job))],
        b"forbidden".to_vec(),
        404,
    )
    .await;
    db::ci_tokens::finish_job(f.pool(), run, "ecosystem-worker", "publish")
        .await
        .unwrap();
    f.execute(
        "UPDATE actions_github_tokens SET revoked_at=0 WHERE run_id=$1",
        &[run],
    )
    .await;
    expect(
        &router,
        "POST",
        &format!("{ruby}/api/v1/gems"),
        &[("authorization", job)],
        gem("job-gem", "2.0.0", "ruby"),
        401,
    )
    .await;
    expect(
        &router,
        "PUT",
        &publish,
        &[("x-nuget-apikey", job)],
        nupkg("Job.Library", "2.0.0"),
        401,
    )
    .await;

    // Retire job-created packages so the per-type list below contains one package.
    for (kind, name) in [
        ("nuget", "job.library"),
        ("rubygems", "job-gem"),
        ("nuget", "base.library"),
        ("rubygems", "rack"),
    ] {
        github(
            &router,
            "packages/delete-package-for-user",
            "DELETE",
            &format!("/api/v3/users/ecosystem-owner/packages/{kind}/{name}"),
            &writer,
            204,
        )
        .await;
    }
    lifecycle(
        &router,
        &f,
        &writer,
        &reader,
        alice,
        "maven",
        "com.example:demo",
        &snapshot,
    )
    .await;
    lifecycle(
        &router,
        &f,
        &writer,
        &reader,
        alice,
        "nuget",
        "example.library",
        &download,
    )
    .await;
    lifecycle(
        &router,
        &f,
        &writer,
        &reader,
        alice,
        "rubygems",
        "demo-gem",
        &gem_download,
    )
    .await;
    // Replaced SNAPSHOT bytes are no longer pinned by that still-active version.
    f.execute("UPDATE package_blob_grants SET expires_at=0", &[])
        .await;
    f.execute("UPDATE package_blobs SET expires_at=0", &[])
        .await;
    let store = codeyard::backend::storage::ObjectStore::open(root.path(), None)
        .await
        .unwrap();
    packages::collect(f.pool(), &store).await.unwrap();
    assert_eq!(
        f.scalar(
            &format!(
                "SELECT COUNT(*) FROM package_blobs WHERE digest='{}'",
                packages::digest(b"snapshot old")
            ),
            &[]
        )
        .await,
        0
    );
    expect(&router, "GET", &snapshot, &read_headers, vec![], 200).await;
    let version=f.scalar("SELECT v.id FROM package_versions v JOIN packages p ON p.id=v.package_id WHERE p.package_type='maven' AND v.name='1.1-SNAPSHOT'",&[]).await;
    f.execute(
        "UPDATE package_versions SET deleted_at=1 WHERE id=$1",
        &[version],
    )
    .await;
    packages::collect(f.pool(), &store).await.unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM package_files WHERE version_id=$1",
            &[version]
        )
        .await,
        0
    );
    expect(&router, "GET", &snapshot, &read_headers, vec![], 404).await;
}

#[tokio::test]
async fn packages_ecosystems_sqlite() {
    ecosystems(false).await;
}
#[tokio::test]
async fn packages_ecosystems_postgres() {
    ecosystems(true).await;
}
