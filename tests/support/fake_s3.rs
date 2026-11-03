//! Bucket startup only; object operations must fail rather than pretend to store bytes.
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{header, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use codeyard::backend::{db, storage::s3::S3Config};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Bucket {
    exists: bool,
    cors: Option<Bytes>,
}

pub struct FakeS3 {
    pub config: S3Config,
    task: tokio::task::JoinHandle<()>,
}

impl FakeS3 {
    pub async fn for_database(pool: &db::DbPool) -> Option<Self> {
        if db::cluster::cluster_enabled(pool).await.unwrap() {
            Some(Self::start().await)
        } else {
            None
        }
    }

    pub fn environment(&self) -> [(&'static str, &str); 5] {
        [
            ("S3_ENDPOINT", &self.config.endpoint),
            ("S3_PUBLIC_ENDPOINT", self.config.get_public_endpoint()),
            ("S3_ACCESS_KEY", &self.config.access_key),
            ("S3_SECRET_KEY", &self.config.secret_key),
            ("S3_BUCKET", &self.config.bucket),
        ]
    }

    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .fallback(bucket_request)
            .layer(DefaultBodyLimit::max(16 * 1024))
            .with_state(Arc::new(Mutex::new(Bucket::default())));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            config: S3Config {
                endpoint: format!("http://{address}"),
                // Cookies ignore ports, so signing needs a different host from PUBLIC_URL.
                public_endpoint: Some(format!("http://localhost:{}", address.port())),
                access_key: "test-access-key".into(),
                secret_key: "test-secret-key".into(),
                bucket: "codeyard-test".into(),
                region: "us-east-1".into(),
            },
            task,
        }
    }
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/xml")],
        format!("<Error><Code>{code}</Code></Error>"),
    )
        .into_response()
}

async fn bucket_request(
    State(bucket): State<Arc<Mutex<Bucket>>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    if uri.path() != "/codeyard-test" && uri.path() != "/codeyard-test/" {
        return error(StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    }
    let mut bucket = bucket.lock().unwrap();
    match (method, uri.query()) {
        (Method::HEAD, None) => {
            if bucket.exists {
                StatusCode::OK.into_response()
            } else {
                error(StatusCode::NOT_FOUND, "NoSuchBucket")
            }
        }
        (Method::PUT, None) => {
            bucket.exists = true;
            StatusCode::OK.into_response()
        }
        (Method::GET | Method::PUT, Some("cors" | "cors=")) if !bucket.exists => {
            error(StatusCode::NOT_FOUND, "NoSuchBucket")
        }
        (Method::GET, Some("cors" | "cors=")) => match &bucket.cors {
            Some(cors) => {
                ([(header::CONTENT_TYPE, "application/xml")], cors.clone()).into_response()
            }
            None => error(StatusCode::NOT_FOUND, "NoSuchCORSConfiguration"),
        },
        (Method::PUT, Some("cors" | "cors=")) => {
            bucket.cors = Some(body);
            StatusCode::OK.into_response()
        }
        _ => error(StatusCode::NOT_IMPLEMENTED, "NotImplemented"),
    }
}
