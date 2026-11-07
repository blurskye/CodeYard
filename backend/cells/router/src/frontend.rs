//! Public HTTP entry point. Directory routes select targets; cells retain authentication.
use crate::{
    directory::Failure,
    graphql_dispatch::Dispatcher,
    http::Transport,
    routes::{ReadyCell, Routes},
    selection::{Catalog, Credential},
};
use anyhow::{ensure, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{header, Method, Request, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use codeyard_model::{
    cell_routes::{Route, Rule},
    cells::{Key, KeyKind, ObjectKind},
};
use codeyard_support::cells::{Prefix, Secret};
use serde_json::{json, Value};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct Config {
    pub region: String,
    pub maximum_control_bytes: usize,
    pub control_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            region: "local".into(),
            maximum_control_bytes: 1024 * 1024,
            control_seconds: 10,
        }
    }
}
pub struct Frontend {
    routes: Routes,
    transport: Transport,
    catalog: Catalog,
    graphql: Dispatcher,
    config: Config,
}
impl Frontend {
    pub fn new(
        routes: Routes,
        transport: Transport,
        catalog: Vec<Route>,
        config: Config,
    ) -> Result<Self> {
        let transport = transport.in_region(&config.region)?;
        ensure!(
            (1024..=8 * 1024 * 1024).contains(&config.maximum_control_bytes)
                && (1..=60).contains(&config.control_seconds),
            "Invalid router request limits"
        );
        Ok(Self {
            graphql: Dispatcher::new(routes.clone(), transport.clone(), Default::default())?,
            routes,
            transport,
            catalog: Catalog::new(catalog)?,
            config,
        })
    }
    pub fn router(self) -> Router {
        Router::new().fallback(entry).with_state(Arc::new(self))
    }
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }
    async fn control(
        &self,
        request: Request<Body>,
    ) -> Result<(Request<Body>, BTreeMap<String, String>)> {
        let (parts, body) = request.into_parts();
        let bytes = tokio::time::timeout(
            Duration::from_secs(self.config.control_seconds),
            to_bytes(body, self.config.maximum_control_bytes),
        )
        .await
        .map_err(|_| Failure::Invalid)?
        .map_err(|_| Failure::Invalid)?;
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let fields = if content_type.split(';').next() == Some("application/json") {
            let object: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
            let object = object.as_object().ok_or(Failure::Invalid)?;
            ensure!(object.len() <= 100, Failure::Invalid);
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        } else if content_type.split(';').next() == Some("application/x-www-form-urlencoded") {
            fields(&bytes)?
        } else {
            anyhow::bail!(Failure::Invalid)
        };
        Ok((Request::from_parts(parts, Body::from(bytes)), fields))
    }
    async fn home(&self, credential: Option<&Credential>) -> Result<ReadyCell> {
        match credential {
            Some(credential) => credential.home(&self.routes).await?,
            None => self.routes.cell(Prefix::ZERO).await?,
        }
        .ok_or_else(|| Failure::Unavailable.into())
    }
    async fn handle(&self, mut request: Request<Body>, peer: SocketAddr) -> Result<Response> {
        // Only this listener supplies forwarding information. Client-supplied chains have no authority.
        for name in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-real-ip",
        ] {
            request.headers_mut().remove(name);
        }
        request
            .headers_mut()
            .insert("x-forwarded-for", peer.ip().to_string().parse()?);
        ensure!(request.uri().to_string().len() <= 16384, Failure::Invalid);
        let credential = Credential::from_headers(request.headers(), "session_id")?;
        let Some(matched) = self.catalog.at(request.uri().path())? else {
            return Ok(error(StatusCode::NOT_FOUND, "Not Found"));
        };
        let rule = matched.rule.clone();
        if rule == Rule::Health {
            return Ok((StatusCode::OK, "ok\n").into_response());
        }
        if rule == Rule::GraphQL {
            ensure!(request.method() == Method::POST, Failure::Invalid);
            let (parts, body) = request.into_parts();
            let bytes = tokio::time::timeout(
                Duration::from_secs(self.config.control_seconds),
                to_bytes(body, self.config.maximum_control_bytes),
            )
            .await
            .map_err(|_| Failure::Invalid)?
            .map_err(|_| Failure::Invalid)?;
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return self.graphql.execute(&parts.headers, body).await;
        }
        let owner = match &rule {
            Rule::Name(parameter) => Some(matched.parameter(parameter)?.to_owned()),
            _ => None,
        };
        let target = match rule {
            Rule::Name(_) | Rule::Id { .. } => self.routes.for_parameter(&matched).await?,
            Rule::PackagePath(parameter) => {
                let path = matched.parameter(&parameter)?;
                let owner = path
                    .trim_start_matches('@')
                    .split('/')
                    .next()
                    .ok_or(Failure::Invalid)?;
                if owner.is_empty() {
                    Some(self.home(credential.as_ref()).await?)
                } else {
                    self.routes.by_name(owner).await?
                }
            }
            Rule::Login if request.method() == Method::POST => {
                let (body, fields) = self.control(request).await?;
                request = body;
                let login = fields
                    .get("username")
                    .or_else(|| fields.get("login"))
                    .or_else(|| fields.get("email"))
                    .ok_or(Failure::Invalid)?;
                if request.uri().path() == "/enter"
                    && fields.get("mode").map(String::as_str) == Some("signup")
                {
                    self.routes.place(&self.config.region).await?
                } else if login.contains('@') {
                    self.routes.by_key(KeyKind::Email, login).await?
                } else {
                    self.routes.by_name(login).await?
                }
            }
            Rule::Device | Rule::OAuthCallback => {
                let query = fields(request.uri().query().unwrap_or("").as_bytes())?;
                let mut parameters = query;
                if request.method() == Method::POST {
                    let (body, values) = self.control(request).await?;
                    request = body;
                    parameters.extend(values);
                }
                let prefix = if let Some(code) = parameters.get("user_code") {
                    codeyard_support::cells::device_code_prefix(code)
                } else if let Some(state) = parameters.get("state") {
                    Secret::cookie(state).ok().map(|value| value.cell)
                } else {
                    credential.as_ref().map(|credential| credential.prefix)
                };
                match prefix {
                    Some(prefix) => self.routes.cell(prefix).await?,
                    None => Some(self.home(None).await?),
                }
            }
            Rule::Signup | Rule::Login => match credential.as_ref() {
                Some(credential) => Some(self.home(Some(credential)).await?),
                None => self.routes.place(&self.config.region).await?,
            },
            Rule::Credential | Rule::Global | Rule::OpaqueId(_) => {
                Some(self.home(credential.as_ref()).await?)
            }
            Rule::GraphQL | Rule::Health => unreachable!("handled above"),
        };
        let Some(target) = target else {
            return Ok(error(StatusCode::NOT_FOUND, "Not Found"));
        };
        let credential = credential
            .as_ref()
            .map_or("", |credential| credential.value());
        let reads = reads_only(&request, self.config.maximum_control_bytes);
        if let Some(nearby) = self.nearby(owner.as_deref(), &target, reads).await {
            return self.read_nearby(nearby, &target, credential, request).await;
        }
        self.transport.forward(&target, credential, request).await
    }
    /// An organization's reads go to its home cell's web nodes in this router's region when it
    /// has presence here. Writes, upgrades and Git negotiations of unknown size go home, and so
    /// does every request when the directory cannot answer.
    async fn nearby(
        &self,
        owner: Option<&str>,
        target: &ReadyCell,
        reads: bool,
    ) -> Option<ReadyCell> {
        if !reads {
            return None;
        }
        let nearby = target.reads_in(&self.config.region)?;
        let key = Key::new(KeyKind::Name, owner?).ok()?;
        let location = self.routes.location(&key).await.ok()??;
        if location.object.kind != ObjectKind::Organization {
            return None;
        }
        let regions = self.routes.presence(location.object.id).await.ok()?;
        regions.contains(&self.config.region).then_some(nearby)
    }
    /// A read is repeated at the home cell when the regional web nodes cannot answer it.
    async fn read_nearby(
        &self,
        nearby: ReadyCell,
        home: &ReadyCell,
        credential: &str,
        request: Request<Body>,
    ) -> Result<Response> {
        let (parts, body) = request.into_parts();
        let body = tokio::time::timeout(
            Duration::from_secs(self.config.control_seconds),
            to_bytes(body, self.config.maximum_control_bytes),
        )
        .await
        .map_err(|_| Failure::Invalid)?
        .map_err(|_| Failure::Invalid)?;
        let copy = || {
            let mut request = Request::new(Body::from(body.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.version_mut() = parts.version;
            *request.headers_mut() = parts.headers.clone();
            request
        };
        match self.transport.forward(&nearby, credential, copy()).await {
            Ok(response)
                if !matches!(
                    response.status(),
                    StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::GATEWAY_TIMEOUT
                ) =>
            {
                Ok(response)
            }
            _ => {
                tracing::debug!(cell = %home.id, region = %self.config.region, "A regional read went to the home cell");
                self.transport.forward(home, credential, copy()).await
            }
        }
    }
}
/// GET and HEAD without a body, or a Git fetch negotiation of a known, bounded size.
fn reads_only(request: &Request<Body>, limit: usize) -> bool {
    let headers = request.headers();
    if headers.contains_key(header::UPGRADE) || headers.contains_key(header::TRANSFER_ENCODING) {
        return false;
    }
    let length = match headers.get(header::CONTENT_LENGTH) {
        None => Some(0),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok()),
    };
    match *request.method() {
        Method::GET | Method::HEAD => length == Some(0),
        Method::POST => {
            request.uri().path().ends_with("/git-upload-pack")
                && length.is_some_and(|length| length > 0 && length <= limit)
        }
        _ => false,
    }
}
fn fields(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(bytes) {
        ensure!(
            fields.len() < 100
                && key.len() <= 100
                && value.len() <= 16384
                && !fields.contains_key(key.as_ref()),
            Failure::Invalid
        );
        fields.insert(key.into_owned(), value.into_owned());
    }
    Ok(fields)
}
fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"message":message,"documentation_url":"https://docs.github.com/rest"})),
    )
        .into_response()
}
async fn entry(
    State(frontend): State<Arc<Frontend>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response {
    match frontend.handle(request, peer).await {
        Ok(response) => response,
        Err(error_value) => match error_value.downcast_ref::<Failure>() {
            Some(Failure::Invalid) => error(StatusCode::BAD_REQUEST, "Invalid request"),
            Some(Failure::Denied) => error(StatusCode::UNAUTHORIZED, "Bad credentials"),
            _ => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The account's cell is unavailable. Please retry.",
            ),
        },
    }
}
