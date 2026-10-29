use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
#[derive(Clone)]
struct Data {
    archives: Arc<BTreeMap<String, Vec<u8>>>,
    queries: Arc<AtomicUsize>,
    tamper: Arc<AtomicBool>,
}
pub struct Provider {
    pub origin: String,
    pub queries: Arc<AtomicUsize>,
    pub tamper: Arc<AtomicBool>,
    data: Data,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn archive(version: &str) -> Vec<u8> {
    let bytes =
        serde_json::to_vec(&json!({"name":"fixture","version":version,"license":"MIT"})).unwrap();
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, "package/package.json", bytes.as_slice())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}
fn integrity(bytes: &[u8]) -> String {
    format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)))
}
pub fn advisory() -> Vec<u8> {
    serde_json::to_vec(&json!({"id":"GHSA-2345-6789-cfgh","modified":"2026-10-01T00:00:00Z","summary":"A real fixture vulnerability","details":"Review the supported direct patch. <script>not markup</script>","database_specific":{"severity":"HIGH"},"affected":[{"package":{"ecosystem":"npm","name":"fixture"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"1.2.1"}]}]}]})).unwrap()
}
async fn metadata(
    State(data): State<Data>,
    Path((name, version)): Path<(String, String)>,
) -> Response {
    data.queries.fetch_add(1, Ordering::SeqCst);
    if name != "fixture" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(bytes) = data.archives.get(&version) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(json!({"name":"fixture","version":version,"dist":{"tarball":format!("https://registry.npmjs.org/fixture/-/fixture-{version}.tgz"),"integrity":integrity(bytes)}})).into_response()
}
async fn tarball(State(data): State<Data>, Path(file): Path<String>) -> Response {
    let version = file
        .strip_prefix("fixture-")
        .unwrap()
        .strip_suffix(".tgz")
        .unwrap();
    let Some(bytes) = data.archives.get(version) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut bytes = bytes.clone();
    if data.tamper.load(Ordering::SeqCst) {
        bytes.push(b'x');
    }
    ([("content-type", "application/octet-stream")], bytes).into_response()
}
impl Provider {
    pub async fn new() -> Self {
        let archives = Arc::new(BTreeMap::from([
            ("1.2.0".into(), archive("1.2.0")),
            ("1.2.1".into(), archive("1.2.1")),
        ]));
        let queries = Arc::new(AtomicUsize::new(0));
        let tamper = Arc::new(AtomicBool::new(false));
        let data = Data {
            archives,
            queries: queries.clone(),
            tamper: tamper.clone(),
        };
        let router = Router::new()
            .route("/fixture/-/:file", get(tarball))
            .route("/:name/:version", get(metadata))
            .with_state(data.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            origin,
            queries,
            tamper,
            data,
            task,
        }
    }
    pub fn manifest(&self, version: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"name":"project","version":"1.0.0","dependencies":{"fixture":format!("^{version}")}})).unwrap()
    }
    pub fn lock(&self, version: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"name":"project","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"project","version":"1.0.0","dependencies":{"fixture":format!("^{version}")}},"node_modules/fixture":{"version":version,"resolved":format!("https://registry.npmjs.org/fixture/-/fixture-{version}.tgz"),"integrity":integrity(&self.data.archives[version]),"license":"MIT"}}})).unwrap()
    }
}
