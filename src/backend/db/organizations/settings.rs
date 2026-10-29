//! Organization profile contacts and enforced member/repository policies.
use super::{OrganizationOutcome, OrganizationProfile};
use crate::backend::db::{models::Organization, DbPool};
use anyhow::Result;
use serde_json::{Map, Value};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Settings {
    pub org_id: i64,
    pub company: Option<String>,
    pub email: Option<String>,
    pub twitter_username: Option<String>,
    pub location: Option<String>,
    pub blog: Option<String>,
    pub has_organization_projects: bool,
    pub default_repository_permission: i16,
    pub members_can_create_public_repositories: bool,
    pub members_can_create_private_repositories: bool,
    pub members_can_fork_private_repositories: bool,
    pub web_commit_signoff_required: bool,
    pub deploy_keys_enabled_for_repositories: bool,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Invalid {
    pub field: &'static str,
    pub message: &'static str,
}
fn invalid(field: &'static str, message: &'static str) -> Invalid {
    Invalid { field, message }
}

#[derive(Clone, Debug)]
pub struct Changes {
    pub(in crate::backend::db) profile: OrganizationProfile,
    pub(in crate::backend::db) billing_email: Option<Option<String>>,
    company: Option<Option<String>>,
    email: Option<Option<String>>,
    twitter_username: Option<Option<String>>,
    location: Option<Option<String>>,
    blog: Option<Option<String>>,
    has_organization_projects: Option<bool>,
    default_repository_permission: Option<i16>,
    members_can_create_public_repositories: Option<bool>,
    members_can_create_private_repositories: Option<bool>,
    members_can_fork_private_repositories: Option<bool>,
    web_commit_signoff_required: Option<bool>,
    deploy_keys_enabled_for_repositories: Option<bool>,
}
fn text(
    input: &Map<String, Value>,
    key: &'static str,
    max: usize,
) -> std::result::Result<Option<Option<String>>, Invalid> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value))
            if value.len() <= max
                && !value
                    .chars()
                    .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t') =>
        {
            Ok(Some((!value.is_empty()).then(|| value.clone())))
        }
        _ => Err(invalid(
            key,
            "Enter bounded text without control characters",
        )),
    }
}
fn flag(
    input: &Map<String, Value>,
    key: &'static str,
) -> std::result::Result<Option<bool>, Invalid> {
    input
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| invalid(key, "Enter a boolean"))
        })
        .transpose()
}
impl Changes {
    pub fn parse(input: &Value) -> std::result::Result<Self, Invalid> {
        let input = input
            .as_object()
            .ok_or_else(|| invalid("organization", "Enter an organization settings object"))?;
        let name = text(input, "name", 1024)?;
        let description = text(input, "description", 4096)?;
        if description
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_some_and(|value| value.chars().count() > 160)
        {
            return Err(invalid("description", "Use at most 160 characters"));
        }
        let profile = OrganizationProfile::partial(
            name.as_ref().map(|value| value.as_deref()),
            description.as_ref().map(|value| value.as_deref()),
        )
        .map_err(|error| {
            invalid(
                if matches!(error, super::OrganizationInputError::Name) {
                    "name"
                } else {
                    "description"
                },
                "Invalid organization profile",
            )
        })?;
        let billing_email = text(input, "billing_email", 254)?;
        let email = text(input, "email", 254)?;
        for (field, value) in [("billing_email", &billing_email), ("email", &email)] {
            if value
                .as_ref()
                .and_then(|value| value.as_ref())
                .is_some_and(|value| {
                    value.contains(['\r', '\n', '\t']) || value.parse::<lettre::Address>().is_err()
                })
            {
                return Err(invalid(field, "Enter a valid email address"));
            }
        }
        let blog = text(input, "blog", 1024)?;
        if let Some(Some(value)) = &blog {
            if crate::website::parse(value).is_none() {
                return Err(invalid("blog", "Enter an absolute HTTP or HTTPS URL"));
            }
        }
        let twitter_username = text(input, "twitter_username", 15)?;
        if twitter_username
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_some_and(|value| {
                !value
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
            })
        {
            return Err(invalid(
                "twitter_username",
                "Enter a username without the @ prefix",
            ));
        }
        // Enterprise-only visibility and Pages have no corresponding native resource.
        for field in [
            "members_can_create_internal_repositories",
            "members_can_create_pages",
            "members_can_create_public_pages",
            "members_can_create_private_pages",
            "has_repository_projects",
        ] {
            if flag(input, field)? == Some(true) {
                return Err(invalid(
                    field,
                    "This repository capability is not available in Codeyard",
                ));
            }
        }
        // Do not claim to configure a scanner without its native configuration and admission path.
        for field in [
            "advanced_security_enabled_for_new_repositories",
            "dependabot_alerts_enabled_for_new_repositories",
            "dependabot_security_updates_enabled_for_new_repositories",
            "dependency_graph_enabled_for_new_repositories",
            "secret_scanning_enabled_for_new_repositories",
            "secret_scanning_push_protection_enabled_for_new_repositories",
            "secret_scanning_push_protection_custom_link_enabled",
            "secret_scanning_push_protection_custom_link",
        ] {
            if input.contains_key(field) {
                return Err(invalid(
                    field,
                    "Configure the native repository security product instead",
                ));
            }
        }
        let default_repository_permission = match input.get("default_repository_permission") {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "none" => 0,
                "read" => 1,
                "write" => 3,
                "admin" => 5,
                _ => {
                    return Err(invalid(
                        "default_repository_permission",
                        "Choose none, read, write or admin",
                    ))
                }
            }),
            _ => {
                return Err(invalid(
                    "default_repository_permission",
                    "Choose none, read, write or admin",
                ))
            }
        };
        let coarse = flag(input, "members_can_create_repositories")?;
        let mut public = coarse;
        let mut private = coarse;
        if let Some(value) = input.get("members_allowed_repository_creation_type") {
            match value.as_str() {
                Some("all") => {
                    public = Some(true);
                    private = Some(true);
                }
                Some("private") => {
                    public = Some(false);
                    private = Some(true);
                }
                Some("none") => {
                    public = Some(false);
                    private = Some(false);
                }
                _ => {
                    return Err(invalid(
                        "members_allowed_repository_creation_type",
                        "Choose all, private or none",
                    ))
                }
            }
        }
        public = flag(input, "members_can_create_public_repositories")?.or(public);
        private = flag(input, "members_can_create_private_repositories")?.or(private);
        Ok(Self {
            profile,
            billing_email,
            company: text(input, "company", 1024)?,
            email,
            twitter_username,
            location: text(input, "location", 1024)?,
            blog,
            has_organization_projects: flag(input, "has_organization_projects")?,
            default_repository_permission,
            members_can_create_public_repositories: public,
            members_can_create_private_repositories: private,
            members_can_fork_private_repositories: flag(
                input,
                "members_can_fork_private_repositories",
            )?,
            web_commit_signoff_required: flag(input, "web_commit_signoff_required")?,
            deploy_keys_enabled_for_repositories: flag(
                input,
                "deploy_keys_enabled_for_repositories",
            )?,
        })
    }
    pub(in crate::backend::db) fn apply(&self, current: &mut Settings) {
        macro_rules! apply {($($field:ident),+)=>{$(if let Some(value)=&self.$field {current.$field=value.clone();})+};}
        apply!(company, email, twitter_username, location, blog);
        macro_rules! flags {($($field:ident),+)=>{$(if let Some(value)=self.$field {current.$field=value;})+};}
        flags!(
            has_organization_projects,
            default_repository_permission,
            members_can_create_public_repositories,
            members_can_create_private_repositories,
            members_can_fork_private_repositories,
            web_commit_signoff_required,
            deploy_keys_enabled_for_repositories
        );
    }
}
/// A sign-off is an explicit trailer, not an automatic attribution or text quoted in the body.
pub fn required_signoff_message(name: &str, email: &str) -> String {
    format!("Organization policy requires a sign-off. Add a final trailer: Signed-off-by: {name} <{email}>")
}

pub fn has_signoff(message: &str, name: &str, email: &str) -> bool {
    let normalized = message.replace("\r\n", "\n");
    let footer = normalized.trim_end().rsplit("\n\n").next().unwrap_or("");
    let expected = format!("{name} <{email}>");
    let mut signed = false;
    for line in footer.lines() {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        {
            return false;
        }
        if key.eq_ignore_ascii_case("Signed-off-by") && value.trim() == expected {
            signed = true;
        }
    }
    signed
}
#[derive(Debug)]
pub struct Updated {
    pub organization: Organization,
    pub settings: Settings,
}

native_operation!(crate::backend::db::postgres::organizations::settings::get,crate::backend::db::sqlite::organizations::settings::get,
    fn get(pool:&DbPool,org:i64)->Result<Option<Settings>>);
native_operation!(crate::backend::db::postgres::organizations::settings::repository_requires_web_signoff,crate::backend::db::sqlite::organizations::settings::repository_requires_web_signoff,
    fn repository_requires_web_signoff(pool:&DbPool,repo:i64)->Result<Option<bool>>);
native_operation!(crate::backend::db::postgres::organizations::settings::owned,crate::backend::db::sqlite::organizations::settings::owned,
    fn owned(pool:&DbPool,actor:i64,org:i64)->Result<OrganizationOutcome<Updated>>);
native_operation!(crate::backend::db::postgres::organizations::settings::change,crate::backend::db::sqlite::organizations::settings::change,
    fn change(pool:&DbPool,actor:i64,org:i64,input:&Changes)->Result<OrganizationOutcome<Updated>>);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn replacement_creation_flags_override_coarse_and_legacy_flags() {
        let changes=Changes::parse(&json!({"members_can_create_repositories":false,"members_allowed_repository_creation_type":"private","members_can_create_public_repositories":true})).unwrap();
        assert_eq!(changes.members_can_create_public_repositories, Some(true));
        assert_eq!(changes.members_can_create_private_repositories, Some(true));
    }
    #[test]
    fn signoff_requires_the_actor_in_the_final_trailer_block() {
        assert!(has_signoff(
            "Update\n\nSigned-off-by: alice <alice@example.test>",
            "alice",
            "alice@example.test"
        ));
        assert!(has_signoff(
            "Update\r\n\r\nsigned-off-by: alice <alice@example.test>\r\n",
            "alice",
            "alice@example.test"
        ));
        for message in [
            "Update",
            "Update\n\nSigned-off-by: bob <alice@example.test>",
            "Update\n\nSigned-off-by: alice <alice@example.test>\n\nThis is quoted text",
            "Update\n\nExample of a trailer\nSigned-off-by: alice <alice@example.test>",
        ] {
            assert!(!has_signoff(message, "alice", "alice@example.test"));
        }
    }
    #[test]
    fn invalid_settings_are_rejected_before_any_effect() {
        for value in [
            json!([]),
            json!({"default_repository_permission":"owner"}),
            json!({"members_can_create_repositories":"true"}),
            json!({"email":"bad\n@example.test"}),
            json!({"blog":"javascript:alert(1)"}),
            json!({"has_repository_projects":true}),
            json!({"members_can_create_internal_repositories":true}),
            json!({"secret_scanning_enabled_for_new_repositories":true}),
        ] {
            assert!(Changes::parse(&value).is_err(), "{value}");
        }
        assert!(Changes::parse(&json!({"description":"a".repeat(161)})).is_err());
        assert!(Changes::parse(&json!({"blog":"https://example.test/path","billing_email":"billing@example.test","name":null,"company":""})).is_ok());
    }
}
