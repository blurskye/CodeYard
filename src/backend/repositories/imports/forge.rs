//! Bounded forge metadata reads. Upstream pagination links never choose a request destination.
use super::Credentials;
use crate::backend::webhooks::outbound;
use anyhow::{ensure, Context, Result};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Forge {
    Github,
    Gitlab,
    Gitea,
    Forgejo,
}

pub struct Project {
    pub forge: Forge,
    api: Url,
}

impl Project {
    pub fn parse(source: &str, forge: Forge) -> Result<Self> {
        let source = super::source_url(source)?;
        Self::from_url(&source, forge)
    }

    fn from_url(source: &Url, forge: Forge) -> Result<Self> {
        let path = source.path().trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<_> = path.split('/').collect();
        ensure!(
            parts.len() >= 2 && parts.iter().all(|part| !part.is_empty()),
            "Invalid forge repository path"
        );
        let mut api = source.clone();
        api.set_query(None);
        api.set_fragment(None);
        match forge {
            Forge::Github => {
                ensure!(
                    parts.len() == 2,
                    "GitHub source needs an owner and repository"
                );
                if source.host_str() == Some("github.com") && source.port().is_none() {
                    api = Url::parse(&format!("https://api.github.com/repos/{path}"))?;
                } else {
                    api.set_path(&format!("/api/v3/repos/{path}"));
                }
            }
            Forge::Gitlab => {
                api.set_path(&format!("/api/v4/projects/{}", path.replace('/', "%2F")))
            }
            Forge::Gitea | Forge::Forgejo => {
                ensure!(
                    parts.len() == 2,
                    "Forge source needs an owner and repository"
                );
                api.set_path(&format!("/api/v1/repos/{path}"));
            }
        }
        Ok(Self { forge, api })
    }

    fn endpoint(&self, suffix: &str) -> Url {
        let mut url = self.api.clone();
        url.set_path(&format!("{}{suffix}", self.api.path()));
        url
    }
}

/// Values retain the source forge's fields; the domain importer performs explicit conversion.
pub struct Metadata {
    pub forge: Forge,
    pub repository: Value,
    pub records: BTreeMap<String, Vec<Value>>,
}

struct Reader<'a> {
    project: Project,
    client: Client,
    credentials: Option<&'a Credentials>,
    bytes: usize,
    requests: usize,
    records: usize,
}

impl Reader<'_> {
    async fn response(&mut self, url: Url) -> Result<Response> {
        ensure!(
            self.requests < 4096,
            "Forge metadata exceeds its request limit"
        );
        self.requests += 1;
        let mut request = self.client.get(url).header("Accept", "application/json");
        if let Some(credentials) = self.credentials {
            request = if self.project.forge == Forge::Gitlab {
                if credentials.username == "oauth2" {
                    request.bearer_auth(&credentials.password)
                } else {
                    let mut token = reqwest::header::HeaderValue::from_str(&credentials.password)?;
                    token.set_sensitive(true);
                    request.header("PRIVATE-TOKEN", token)
                }
            } else if self.project.forge == Forge::Github {
                request.bearer_auth(&credentials.password)
            } else {
                request.basic_auth(&credentials.username, Some(&credentials.password))
            };
        }
        let response = request.send().await?;
        ensure!(
            response.status().is_success(),
            "Source forge refused metadata access"
        );
        Ok(response)
    }

    async fn value(&mut self, mut response: Response) -> Result<Value> {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= 8 * 1024 * 1024 - bytes.len()
                    && chunk.len() <= 64 * 1024 * 1024 - self.bytes,
                "Forge metadata exceeds its size limit"
            );
            self.bytes += chunk.len();
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn object(&mut self, suffix: &str) -> Result<Value> {
        let response = self.response(self.project.endpoint(suffix)).await?;
        let value = self.value(response).await?;
        ensure!(value.is_object(), "Invalid forge metadata object");
        ensure!(self.records < 50_000, "Too many forge metadata records");
        self.records += 1;
        Ok(value)
    }

    async fn list(&mut self, suffix: &str, query: &[(&str, &str)]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        for page in 1..=4096 {
            let mut url = self.project.endpoint(suffix);
            let limit = if matches!(self.project.forge, Forge::Gitea | Forge::Forgejo) {
                "limit"
            } else {
                "per_page"
            };
            url.query_pairs_mut()
                .extend_pairs(query.iter().copied())
                .append_pair(limit, "100")
                .append_pair("page", &page.to_string());
            let response = self.response(url).await?;
            let linked = response
                .headers()
                .get("link")
                .and_then(|header| header.to_str().ok())
                .is_some_and(|links| {
                    links.split(',').any(|link| {
                        link.split(';').skip(1).any(|attribute| {
                            matches!(attribute.trim(), "rel=\"next\"" | "rel=next")
                        })
                    })
                });
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|header| header.to_str().ok())
                .is_some_and(|page| !page.is_empty());
            let total = response
                .headers()
                .get("x-total-count")
                .and_then(|header| header.to_str().ok())
                .and_then(|total| total.parse::<usize>().ok());
            let value = self.value(response).await?;
            let values = value.as_array().context("Invalid forge metadata list")?;
            ensure!(
                values.len() <= 100 && values.iter().all(Value::is_object),
                "Invalid forge metadata list"
            );
            ensure!(
                values.len() <= 50_000 - self.records,
                "Too many forge metadata records"
            );
            self.records += values.len();
            let more = linked
                || next
                || total.is_some_and(|total| output.len() + values.len() < total)
                || values.len() == 100;
            ensure!(!more || !values.is_empty(), "Invalid forge pagination");
            output.extend(values.iter().cloned());
            if !more {
                return Ok(output);
            }
        }
        anyhow::bail!("Forge metadata exceeds its page limit")
    }
}

fn number(value: &Value, forge: Forge) -> Result<i64> {
    value[if forge == Forge::Gitlab {
        "iid"
    } else {
        "number"
    }]
    .as_i64()
    .filter(|number| *number > 0)
    .context("Invalid source issue number")
}

fn children(
    records: &mut BTreeMap<String, Vec<Value>>,
    kind: &str,
    parent_kind: &str,
    parent: i64,
    values: Vec<Value>,
) {
    records
        .entry(kind.to_owned())
        .or_default()
        .extend(values.into_iter().map(|mut value| {
            value["source_parent_kind"] = Value::String(parent_kind.to_owned());
            value["source_parent_number"] = Value::from(parent);
            value
        }));
}

pub async fn fetch(project: Project, credentials: Option<&Credentials>) -> Result<Metadata> {
    ensure!(
        credentials.is_none_or(Credentials::valid),
        "Invalid source credentials"
    );
    tokio::time::timeout(Duration::from_secs(900), fetch_inner(project, credentials)).await?
}

async fn fetch_inner(project: Project, credentials: Option<&Credentials>) -> Result<Metadata> {
    let addresses = tokio::time::timeout(
        Duration::from_secs(15),
        outbound::resolve(&project.api, super::allow_private()),
    )
    .await?
    .map_err(|_| anyhow::anyhow!("Source forge address refused"))?;
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent("Codeyard repository migration");
    if let Some(url::Host::Domain(host)) = project.api.host() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let forge = project.forge;
    let mut reader = Reader {
        project,
        client: builder.build()?,
        credentials,
        bytes: 0,
        requests: 0,
        records: 0,
    };
    let repository = reader.object("").await?;
    let mut records = BTreeMap::new();
    records.insert("label".to_owned(), reader.list("/labels", &[]).await?);
    let milestone_query: &[(&str, &str)] = if forge == Forge::Gitlab {
        &[]
    } else {
        &[("state", "all")]
    };
    records.insert(
        "milestone".to_owned(),
        reader.list("/milestones", milestone_query).await?,
    );
    let item_query: &[(&str, &str)] = match forge {
        Forge::Gitlab => &[
            ("state", "all"),
            ("order_by", "created_at"),
            ("sort", "asc"),
        ],
        Forge::Github => &[("state", "all"), ("sort", "created"), ("direction", "asc")],
        Forge::Gitea | Forge::Forgejo => &[("state", "all")],
    };
    let issues = reader.list("/issues", item_query).await?;
    let issues: Vec<_> = issues
        .into_iter()
        .filter(|issue| issue["pull_request"].is_null())
        .collect();
    if forge == Forge::Gitlab {
        for issue in &issues {
            let id = number(issue, forge)?;
            let values = reader
                .list(&format!("/issues/{id}/notes"), &[("sort", "asc")])
                .await?;
            children(&mut records, "issue_comment", "issue", id, values);
        }
    } else {
        let values = reader.list("/issues/comments", &[]).await?;
        records.insert("issue_comment".to_owned(), values);
    }
    records.insert("issue".to_owned(), issues);
    let pulls_path = if forge == Forge::Gitlab {
        "/merge_requests"
    } else {
        "/pulls"
    };
    let pulls = reader.list(pulls_path, item_query).await?;
    let mut detailed = Vec::new();
    for pull in pulls {
        let id = number(&pull, forge)?;
        let path = format!("{pulls_path}/{id}");
        detailed.push(reader.object(&path).await?);
        if forge == Forge::Gitlab {
            let notes = reader
                .list(&format!("{path}/notes"), &[("sort", "asc")])
                .await?;
            children(
                &mut records,
                "pull_request_comment",
                "pull_request",
                id,
                notes,
            );
            let approvals = reader.object(&format!("{path}/approvals")).await?;
            children(
                &mut records,
                "approval",
                "pull_request",
                id,
                vec![approvals],
            );
        } else {
            let reviews = reader.list(&format!("{path}/reviews"), &[]).await?;
            ensure!(
                reviews.iter().all(|review| review["state"].is_string()
                    && review["id"].as_i64().is_some_and(|id| id > 0)),
                "Invalid source review"
            );
            let pending: std::collections::HashSet<_> = reviews
                .iter()
                .filter(|review| {
                    review["state"]
                        .as_str()
                        .is_some_and(|state| state.eq_ignore_ascii_case("pending"))
                })
                .filter_map(|review| review["id"].as_i64())
                .collect();
            let reviews: Vec<_> = reviews
                .into_iter()
                .filter(|review| {
                    review["state"]
                        .as_str()
                        .is_none_or(|state| !state.eq_ignore_ascii_case("pending"))
                })
                .collect();
            if forge == Forge::Github {
                let comments = reader.list(&format!("{path}/comments"), &[]).await?;
                let comments = comments
                    .into_iter()
                    .filter(|comment| {
                        !comment["pull_request_review_id"]
                            .as_i64()
                            .is_some_and(|id| pending.contains(&id))
                    })
                    .collect();
                children(
                    &mut records,
                    "pull_request_review_comment",
                    "pull_request",
                    id,
                    comments,
                );
            } else {
                for review in &reviews {
                    let review_id = review["id"]
                        .as_i64()
                        .filter(|id| *id > 0)
                        .context("Invalid source review")?;
                    let comments = reader
                        .list(&format!("{path}/reviews/{review_id}/comments"), &[])
                        .await?;
                    children(
                        &mut records,
                        "pull_request_review_comment",
                        "pull_request",
                        id,
                        comments,
                    );
                }
            }
            children(
                &mut records,
                "pull_request_review",
                "pull_request",
                id,
                reviews,
            );
        }
    }
    records.insert("pull_request".to_owned(), detailed);
    records.insert("release".to_owned(), reader.list("/releases", &[]).await?);
    Ok(Metadata {
        forge,
        repository,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_forge_api_roots_do_not_accept_upstream_links() {
        for (source, forge, expected) in [
            (
                "https://github.com/owner/repo.git",
                Forge::Github,
                "https://api.github.com/repos/owner/repo",
            ),
            (
                "https://enterprise.test/owner/repo",
                Forge::Github,
                "https://enterprise.test/api/v3/repos/owner/repo",
            ),
            (
                "https://gitlab.com/group/sub/repo.git",
                Forge::Gitlab,
                "https://gitlab.com/api/v4/projects/group%2Fsub%2Frepo",
            ),
            (
                "https://gitea.test/owner/repo.git",
                Forge::Gitea,
                "https://gitea.test/api/v1/repos/owner/repo",
            ),
            (
                "https://codeberg.org/owner/repo",
                Forge::Forgejo,
                "https://codeberg.org/api/v1/repos/owner/repo",
            ),
        ] {
            let project = Project::parse(source, forge).unwrap();
            assert_eq!(project.api.as_str(), expected);
            assert_eq!(project.endpoint("/issues").origin(), project.api.origin());
        }
        assert!(Project::parse("https://credential:secret@github.com/o/r", Forge::Github).is_err());
        assert!(Project::parse("https://github.com/o/r?token=secret", Forge::Github).is_err());
        assert!(Project::parse("https://github.com/o/r/extra", Forge::Github).is_err());
    }

    #[tokio::test]
    async fn pagination_cannot_redirect_credentials_or_choose_an_upstream_url() {
        use axum::{
            extract::Query,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        use serde_json::json;
        use std::collections::HashMap;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route(
                "/api/v1/repos/o/r/issues",
                get(
                    |Query(query): Query<HashMap<String, String>>, headers: HeaderMap| async move {
                        if headers
                            .get("authorization")
                            .and_then(|header| header.to_str().ok())
                            != Some("Basic dTpw")
                        {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        let page: i64 = query["page"].parse().unwrap();
                        assert_eq!(query["limit"], "100");
                        let mut response = Json(json!([{"id":page}])).into_response();
                        response
                            .headers_mut()
                            .insert("x-total-count", "2".parse().unwrap());
                        if page == 1 {
                            response.headers_mut().insert(
                                "link",
                                "<http://169.254.169.254/secret>; rel=\"next\""
                                    .parse()
                                    .unwrap(),
                            );
                        }
                        response
                    },
                ),
            )
            .route(
                "/api/v1/repos/o/r/redirect",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "http://169.254.169.254/secret")],
                    )
                }),
            )
            .route(
                "/api/v1/repos/o/r/empty",
                get(|| async {
                    (
                        [("link", "<http://169.254.169.254/secret>; rel=\"next\"")],
                        Json(json!([])),
                    )
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let credentials = Credentials {
            username: "u".to_owned(),
            password: "p".to_owned(),
        };
        let project = Project::from_url(
            &Url::parse(&format!("http://{address}/o/r")).unwrap(),
            Forge::Gitea,
        )
        .unwrap();
        let mut reader = Reader {
            project,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            credentials: Some(&credentials),
            bytes: 0,
            requests: 0,
            records: 0,
        };
        let records = reader.list("/issues", &[]).await.unwrap();
        assert_eq!(records, vec![json!({"id":1}), json!({"id":2})]);
        assert_eq!(reader.requests, 2);
        assert!(reader.list("/redirect", &[]).await.is_err());
        assert!(reader.list("/empty", &[]).await.is_err());
        reader.requests = 4096;
        assert!(reader.list("/issues", &[]).await.is_err());
        server.abort();
    }
}
