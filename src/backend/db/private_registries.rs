//! Organization registry metadata is readable separately from its encrypted credential.
use super::secrets::SecretsError;
use crate::backend::crypto::SecretEncryptor;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub registry_type: String,
    pub url: String,
    pub visibility: String,
    #[serde(default = "token")]
    pub auth_type: String,
    #[serde(default)]
    pub replaces_base: bool,
    pub username: Option<String>,
    pub selected_repository_ids: Option<Vec<i64>>,
    pub tenant_id: Option<String>,
    pub client_id: Option<String>,
    pub aws_region: Option<String>,
    pub account_id: Option<String>,
    pub role_name: Option<String>,
    pub domain: Option<String>,
    pub domain_owner: Option<String>,
    pub jfrog_oidc_provider_name: Option<String>,
    pub audience: Option<String>,
    pub identity_mapping_name: Option<String>,
    pub namespace: Option<String>,
    pub service_slug: Option<String>,
    pub api_host: Option<String>,
    pub workload_identity_provider: Option<String>,
    pub service_account: Option<String>,
}

fn token() -> String {
    "token".to_owned()
}

impl Configuration {
    pub(crate) fn validate(&self, value: Option<&str>, exists: bool) -> Result<(), RegistryError> {
        if !matches!(
            self.registry_type.as_str(),
            "maven_repository"
                | "nuget_feed"
                | "goproxy_server"
                | "npm_registry"
                | "rubygems_server"
                | "cargo_registry"
                | "composer_repository"
                | "docker_registry"
                | "git_source"
                | "helm_registry"
                | "hex_organization"
                | "hex_repository"
                | "pub_repository"
                | "python_index"
                | "terraform_registry"
        ) {
            return Err(RegistryError::Invalid("registry_type"));
        }
        if serde_json::to_vec(self)?.len() > 32 * 1024 {
            return Err(RegistryError::Invalid("configuration"));
        }
        let url = url::Url::parse(&self.url).map_err(|_| RegistryError::Invalid("url"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(RegistryError::Invalid("url"));
        }
        if !matches!(self.visibility.as_str(), "all" | "private" | "selected")
            || (self.selected_repository_ids.is_some() && self.visibility != "selected")
        {
            return Err(RegistryError::Invalid("visibility"));
        }
        if let Some(ids) = &self.selected_repository_ids {
            super::dependabot::validate_repository_access(ids, &[])?;
        }
        let required: Vec<(&str, Option<&str>)> = match self.auth_type.as_str() {
            "token" => vec![],
            "username_password" => vec![("username", self.username.as_deref())],
            "anonymous" => {
                if self.username.is_some() || value.is_some() {
                    return Err(RegistryError::Invalid("auth_type"));
                }
                vec![]
            }
            "oidc_azure" => vec![
                ("tenant_id", self.tenant_id.as_deref()),
                ("client_id", self.client_id.as_deref()),
            ],
            "oidc_aws" => vec![
                ("aws_region", self.aws_region.as_deref()),
                ("account_id", self.account_id.as_deref()),
                ("role_name", self.role_name.as_deref()),
                ("domain", self.domain.as_deref()),
                ("domain_owner", self.domain_owner.as_deref()),
            ],
            "oidc_jfrog" => vec![(
                "jfrog_oidc_provider_name",
                self.jfrog_oidc_provider_name.as_deref(),
            )],
            "oidc_cloudsmith" => vec![
                ("namespace", self.namespace.as_deref()),
                ("service_slug", self.service_slug.as_deref()),
                ("audience", self.audience.as_deref()),
            ],
            "oidc_gcp" => vec![(
                "workload_identity_provider",
                self.workload_identity_provider.as_deref(),
            )],
            _ => return Err(RegistryError::Invalid("auth_type")),
        };
        for (name, field) in required {
            if field.is_none_or(|value| value.trim().is_empty()) {
                return Err(RegistryError::Invalid(name));
            }
        }
        if matches!(self.auth_type.as_str(), "token" | "username_password") {
            if !exists && value.is_none() {
                return Err(RegistryError::Invalid("encrypted_value"));
            }
            if value.is_some_and(|v| v.is_empty() || v.len() > 64 * 1024) {
                return Err(RegistryError::Invalid("encrypted_value"));
            }
        } else if value.is_some() {
            return Err(RegistryError::Invalid("encrypted_value"));
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
pub struct Registry {
    pub name: String,
    pub configuration: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Registry {
    pub fn document(&self) -> Result<Value, RegistryError> {
        let mut value: Value = serde_json::from_str(&self.configuration)?;
        value
            .as_object_mut()
            .ok_or(RegistryError::Invalid("configuration"))?
            .retain(|_, value| !value.is_null());
        value["name"] = serde_json::json!(self.name);
        value["created_at"] = serde_json::json!(self.created_at);
        value["updated_at"] = serde_json::json!(self.updated_at);
        Ok(value)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Invalid private registry {0}")]
    Invalid(&'static str),
    #[error("Private registry not found")]
    Missing,
    #[error("Private registry encryption unavailable")]
    Unavailable,
    #[error("Private registry database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Private registry authorization or credential failure")]
    Secret(#[from] SecretsError),
    #[error("Invalid stored private registry")]
    Stored(#[from] serde_json::Error),
}

pub(crate) fn configuration(
    stored: Option<&str>,
    input: &Value,
) -> Result<Configuration, RegistryError> {
    let input = input
        .as_object()
        .ok_or(RegistryError::Invalid("configuration"))?;
    let mut merged: Value = match stored {
        Some(value) => serde_json::from_str(value)?,
        None => serde_json::json!({}),
    };
    let merged = merged
        .as_object_mut()
        .ok_or(RegistryError::Invalid("configuration"))?;
    for (key, value) in input {
        merged.insert(key.clone(), value.clone());
    }
    // Changing visibility also clears a previously selected list unless another list is supplied.
    if input
        .get("visibility")
        .and_then(Value::as_str)
        .is_some_and(|v| v != "selected")
        && !input.contains_key("selected_repository_ids")
    {
        merged.remove("selected_repository_ids");
    }
    let result: Configuration = serde_json::from_value(Value::Object(merged.clone()))
        .map_err(|_| RegistryError::Invalid("configuration"))?;
    if let Some(stored) = stored {
        let old: Configuration = serde_json::from_str(stored)?;
        if old.auth_type != result.auth_type {
            return Err(RegistryError::Invalid("auth_type"));
        }
    }
    Ok(result)
}

native_operation!(super::postgres::private_registries::list,
    super::sqlite::private_registries::list,
    fn list(pool: &DbPool, actor: i64, org: i64, name: Option<&str>, offset: i64, limit: i64) -> Result<(i64, Vec<Registry>), RegistryError>);

native_operation!(super::postgres::private_registries::save,
    super::sqlite::private_registries::save,
    fn save(pool: &DbPool, encryptor: Option<&SecretEncryptor>, actor: i64, credential: &super::users::accounts::AccountCredential<'_>, org: i64, name: Option<&str>, input: &Value, value: Option<&str>) -> Result<Registry, RegistryError>);

native_operation!(super::postgres::private_registries::delete,
    super::sqlite::private_registries::delete,
    fn delete(pool: &DbPool, actor: i64, credential: &super::users::accounts::AccountCredential<'_>, org: i64, name: &str) -> Result<(), RegistryError>);
