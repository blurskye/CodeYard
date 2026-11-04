//! Pages configuration and failed publication attempts. This server has no Pages publisher.
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const NO_PUBLISHER: &str =
    "Pages hosting and OIDC verification are not configured on this server.";
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Pages administration denied")]
    Denied,
    #[error("Pages site or attempt not found")]
    Missing,
    #[error("Pages site already exists")]
    Duplicate,
    #[error("Invalid Pages settings")]
    Invalid,
    #[error("Pages attempt is no longer cancellable")]
    Conflict,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Site {
    pub repo_id: i64,
    pub build_type: String,
    pub branch: Option<String>,
    pub path: Option<String>,
    pub cname: Option<String>,
    pub https_enforced: bool,
    #[sqlx(rename = "is_public")]
    pub public: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source {
    pub branch: String,
    pub path: String,
}
/// The repository role a Pages write needs. Maintainers configure the site, as on GitHub;
/// deleting it stays with administrators.
#[derive(Clone, Copy)]
pub(in crate::backend::db) enum Role {
    Write,
    Maintain,
    Admin,
}

#[derive(Default)]
pub struct Update {
    pub build_type: Option<String>,
    pub source: Option<Source>,
    pub cname: Option<String>,
    pub set_cname: bool,
    pub https_enforced: Option<bool>,
    pub public: Option<bool>,
}
impl Update {
    pub fn parse(value: &Value, create: bool) -> Result<Self> {
        let map = value.as_object().ok_or(Refusal::Invalid)?;
        anyhow::ensure!(
            map.keys().all(|key| matches!(
                key.as_str(),
                "build_type" | "source" | "cname" | "https_enforced" | "public"
            )) && !map.is_empty(),
            Refusal::Invalid
        );
        let build_type = map
            .get("build_type")
            .map(|v| v.as_str().map(str::to_owned).ok_or(Refusal::Invalid))
            .transpose()?;
        let source = map
            .get("source")
            .map(|value| match value {
                Value::String(value) => match value.as_str() {
                    "gh-pages" => Ok(Source {
                        branch: value.clone(),
                        path: "/".into(),
                    }),
                    "master" => Ok(Source {
                        branch: value.clone(),
                        path: "/".into(),
                    }),
                    "master /docs" => Ok(Source {
                        branch: "master".into(),
                        path: "/docs".into(),
                    }),
                    _ => Err(Refusal::Invalid),
                },
                Value::Object(map) => {
                    if !map
                        .keys()
                        .all(|key| matches!(key.as_str(), "branch" | "path"))
                    {
                        return Err(Refusal::Invalid);
                    }
                    let branch = map
                        .get("branch")
                        .and_then(Value::as_str)
                        .ok_or(Refusal::Invalid)?;
                    let path = match map.get("path") {
                        None if create => "/",
                        Some(Value::String(path)) => path,
                        _ => return Err(Refusal::Invalid),
                    };
                    Ok(Source {
                        branch: branch.into(),
                        path: path.into(),
                    })
                }
                _ => Err(Refusal::Invalid),
            })
            .transpose()?;
        let cname = match map.get("cname") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.trim().to_ascii_lowercase()),
            _ => anyhow::bail!(Refusal::Invalid),
        };
        let flag = |name| {
            map.get(name)
                .map(|v| v.as_bool().ok_or(Refusal::Invalid))
                .transpose()
        };
        let update = Self {
            build_type,
            source,
            cname,
            set_cname: map.contains_key("cname"),
            https_enforced: flag("https_enforced")?,
            public: flag("public")?,
        };
        update.validate(create)?;
        Ok(update)
    }
    pub(crate) fn validate(&self, create: bool) -> Result<()> {
        anyhow::ensure!(
            self.build_type
                .as_deref()
                .is_none_or(|v| matches!(v, "legacy" | "workflow")),
            Refusal::Invalid
        );
        anyhow::ensure!(
            !create || self.source.is_some() || self.build_type.as_deref() == Some("workflow"),
            Refusal::Invalid
        );
        if let Some(source) = &self.source {
            anyhow::ensure!(
                matches!(source.path.as_str(), "/" | "/docs")
                    && codeyard_git::branch_ref(&source.branch).is_some(),
                Refusal::Invalid
            );
        }
        if let Some(domain) = &self.cname {
            anyhow::ensure!(
                domain.len() <= 253
                    && domain.contains('.')
                    && domain.split('.').all(|label| !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                    && domain.parse::<std::net::IpAddr>().is_err(),
                Refusal::Invalid
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Build {
    pub id: i64,
    pub repo_id: i64,
    pub actor_id: Option<i64>,
    pub commit_sha: String,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Deployment {
    pub id: i64,
    pub repo_id: i64,
    pub commit_sha: String,
    pub environment: String,
    pub status: String,
    pub artifact_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

native_operation!(crate::backend::db::postgres::pages::site,crate::backend::db::sqlite::pages::site,
    fn site(pool:&DbPool,reader:&crate::backend::db::repositories::credentials::Reader<'_>,repo:i64)->Result<Option<Site>>);
native_operation!(crate::backend::db::postgres::pages::save,crate::backend::db::sqlite::pages::save,
    fn save(pool:&DbPool,actor:i64,credential:&crate::backend::db::users::accounts::AccountCredential<'_>,repo:i64,update:&Update,create:bool)->Result<Site>);
native_operation!(crate::backend::db::postgres::pages::delete,crate::backend::db::sqlite::pages::delete,
    fn delete(pool:&DbPool,actor:i64,credential:&crate::backend::db::users::accounts::AccountCredential<'_>,repo:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::pages::builds,crate::backend::db::sqlite::pages::builds,
    fn builds(pool:&DbPool,reader:&crate::backend::db::repositories::credentials::Reader<'_>,repo:i64,offset:i64,limit:i64)->Result<Vec<Build>>);
native_operation!(crate::backend::db::postgres::pages::build,crate::backend::db::sqlite::pages::build,
    fn build(pool:&DbPool,reader:&crate::backend::db::repositories::credentials::Reader<'_>,repo:i64,id:i64)->Result<Option<Build>>);
native_operation!(crate::backend::db::postgres::pages::request_build,crate::backend::db::sqlite::pages::request_build,
    fn request_build(pool:&DbPool,actor:i64,credential:&crate::backend::db::users::accounts::AccountCredential<'_>,repo:i64,sha:&str)->Result<Build>);
pub struct Attempt<'a> {
    pub sha: &'a str,
    pub environment: &'a str,
    pub artifact_id: Option<i64>,
}
native_operation!(crate::backend::db::postgres::pages::request_deployment,crate::backend::db::sqlite::pages::request_deployment,
    fn request_deployment(pool:&DbPool,actor:i64,credential:&crate::backend::db::users::accounts::AccountCredential<'_>,repo:i64,input:Attempt<'_>)->Result<Deployment>);
native_operation!(crate::backend::db::postgres::pages::deployment,crate::backend::db::sqlite::pages::deployment,
    fn deployment(pool:&DbPool,reader:&crate::backend::db::repositories::credentials::Reader<'_>,repo:i64,id:i64)->Result<Option<Deployment>>);
native_operation!(crate::backend::db::postgres::pages::cancel,crate::backend::db::sqlite::pages::cancel,
    fn cancel(pool:&DbPool,actor:i64,credential:&crate::backend::db::users::accounts::AccountCredential<'_>,repo:i64,id:i64)->Result<()>);
