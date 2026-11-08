use anyhow::ensure;
use anyhow::Result;
use codeyard_database::models::Repository;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use serde_json::json;
use serde_json::Value;

pub(crate) const SETTINGS_PAGE_SIZE: usize = 30;

pub struct SettingsPage {
    pub organization_name: String,
    pub writable: bool,
    pub configurations: Vec<Configuration>,
    pub next_before: Option<i64>,
}
impl SettingsPage {
    pub fn from_rows(
        organization_name: String,
        writable: bool,
        mut configurations: Vec<Configuration>,
    ) -> Self {
        let more = configurations.len() > SETTINGS_PAGE_SIZE;
        configurations.truncate(SETTINGS_PAGE_SIZE);
        let next_before = more
            .then(|| configurations.last().map(|row| row.id))
            .flatten();
        Self {
            organization_name,
            writable,
            configurations,
            next_before,
        }
    }
}

const OPTIONS: &[&str] = &[
    "code_scanning_options",
    "code_scanning_default_setup_options",
    "dependency_graph_autosubmit_action_options",
    "secret_scanning_delegated_bypass_options",
];

pub fn merge(current: Option<&str>, patch: &Value) -> Result<Value> {
    let mut settings = match current {
        Some(value) => serde_json::from_str(value)?,
        None => {
            json!({"advanced_security":"disabled","dependency_graph":"enabled","enforcement":"enforced"})
        }
    };
    if current.is_none() {
        for key in TOGGLES {
            if settings.get(*key).is_none() {
                settings[*key] = json!("disabled");
            }
        }
    }
    for (key, value) in patch.as_object().ok_or(Refusal::Invalid)? {
        let valid = match key.as_str() {
            "name" => value.as_str().is_some_and(|s| {
                !s.trim().is_empty() && s.len() <= 255 && !s.chars().any(char::is_control)
            }),
            "description" => value.as_str().is_some_and(|s| s.chars().count() <= 255),
            "advanced_security" => value.as_str().is_some_and(|s| {
                ["enabled", "disabled", "code_security", "secret_protection"].contains(&s)
            }),
            "enforcement" => value
                .as_str()
                .is_some_and(|s| ["enforced", "unenforced"].contains(&s)),
            key if TOGGLES.contains(&key) => value
                .as_str()
                .is_some_and(|s| ["enabled", "disabled", "not_set"].contains(&s)),
            key if OPTIONS.contains(&key) => value.is_object() || value.is_null(),
            _ => false,
        };
        ensure!(valid, Refusal::Invalid);
        settings[key] = value.clone();
    }
    ensure!(settings["name"].is_string(), Refusal::Invalid);
    // A stored configuration must not claim that Codeyard runs the CodeQL service.
    ensure!(
        settings["code_scanning_default_setup"] != "enabled"
            && settings["dependabot_security_updates"] != "enabled",
        Refusal::Invalid
    );
    ensure!(
        settings["secret_scanning"] != "disabled"
            || settings["secret_scanning_push_protection"] != "enabled",
        Refusal::Invalid
    );
    // Nor that reviewers approve push protection bypasses: every writer may still bypass.
    ensure!(
        settings["secret_scanning_delegated_bypass"] != "enabled"
            && settings["secret_scanning_delegated_bypass_options"]["reviewers"]
                .as_array()
                .is_none_or(Vec::is_empty),
        Refusal::Invalid
    );
    for (key, fields) in [
        ("code_scanning_options", &["allow_advanced"][..]),
        (
            "code_scanning_default_setup_options",
            &["runner_type", "runner_label"][..],
        ),
        (
            "dependency_graph_autosubmit_action_options",
            &["labeled_runners"][..],
        ),
    ] {
        if let Some(options) = settings[key].as_object() {
            for (name, value) in options {
                ensure!(fields.contains(&name.as_str()), Refusal::Invalid);
                ensure!(
                    match name.as_str() {
                        "runner_type" => value
                            .as_str()
                            .is_some_and(|s| ["standard", "labeled", "not_set"].contains(&s)),
                        "runner_label" => value.is_string() || value.is_null(),
                        _ => value.is_boolean() || value.is_null(),
                    },
                    Refusal::Invalid
                );
            }
        }
    }
    if let Some(options) = settings["secret_scanning_delegated_bypass_options"].as_object() {
        ensure!(
            options.keys().all(|key| key == "reviewers"),
            Refusal::Invalid
        );
        if let Some(reviewers) = options.get("reviewers") {
            for reviewer in reviewers.as_array().ok_or(Refusal::Invalid)? {
                ensure!(
                    reviewer["reviewer_id"].as_i64().is_some_and(|id| id > 0)
                        && reviewer["reviewer_type"]
                            .as_str()
                            .is_some_and(|s| ["TEAM", "ROLE"].contains(&s))
                        && reviewer.get("mode").is_none_or(|v| v
                            .as_str()
                            .is_some_and(|s| ["ALWAYS", "EXEMPT"].contains(&s))),
                    Refusal::Invalid
                );
            }
        }
    }
    Ok(settings)
}

/// The secret scanning settings that an enforced configuration fixes for its repository, or
/// `None` when the configuration is not enforced. A setting it leaves unset stays as `current`.
pub(crate) fn enforced_scanning(
    configuration: &str,
    current: codeyard_database::queries::secret_scanning::Settings,
) -> Result<Option<codeyard_database::queries::secret_scanning::Settings>> {
    let value: Value = serde_json::from_str(configuration)?;
    if value["enforcement"] != "enforced" {
        return Ok(None);
    }
    scanning_settings(&apply(None, configuration)?, current).map(Some)
}

native_operation!(crate::postgres::code_security::list,crate::sqlite::code_security::list,
    fn list(pool:&DbPool,actor:i64,org:i64) -> Result<Vec<Configuration>>);
native_operation!(crate::postgres::code_security::list_window,crate::sqlite::code_security::list_window,
    /// One keyset window of the organization's configurations by id: those after `after` and before `before`, oldest first, or newest first when `before` is bounded.
    fn list_window(pool:&DbPool,actor:i64,org:i64,after:i64,before:i64,limit:i64) -> Result<Vec<Configuration>>);
native_operation!(crate::postgres::code_security::settings_page,crate::sqlite::code_security::settings_page,
    fn settings_page(pool:&DbPool,actor:i64,session:&str,org:i64,before:Option<i64>) -> Result<SettingsPage>);
native_operation!(crate::postgres::code_security::save,crate::sqlite::code_security::save,
    fn save(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,org:i64,id:Option<i64>,patch:&Value) -> Result<Configuration>);
native_operation!(crate::postgres::code_security::delete,crate::sqlite::code_security::delete,
    fn delete(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,org:i64,id:i64) -> Result<()>);
native_operation!(crate::postgres::code_security::set_default,crate::sqlite::code_security::set_default,
    fn set_default(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,org:i64,id:i64,scope:&str) -> Result<Configuration>);
native_operation!(crate::postgres::code_security::attach,crate::sqlite::code_security::attach,
    fn attach(pool:&DbPool,actor:impl Into<codeyard_database::queries::credentials::Actor>,org:i64,id:Option<i64>,scope:&str,selected:&[i64]) -> Result<()>);
native_operation!(codeyard_database::queries::postgres::code_security::repositories,codeyard_database::queries::sqlite::code_security::repositories,
    /// One keyset window of the repositories attached to configuration `id`, ordered as `list_window`.
    fn repositories(pool:&DbPool,actor:i64,org:i64,id:i64,after:i64,before:i64,limit:i64) -> Result<Vec<Repository>>);
native_operation!(crate::postgres::code_security::repository_settings,crate::sqlite::code_security::repository_settings,
    fn repository_settings(pool:&DbPool,repo:i64) -> Result<Option<RepositorySettings>>);
native_operation!(codeyard_database::queries::postgres::code_security::repository_configuration,codeyard_database::queries::sqlite::code_security::repository_configuration,
    fn repository_configuration(pool:&DbPool,repo:i64) -> Result<Option<Configuration>>);

use codeyard_database::queries::code_security::scanning_settings;

use codeyard_database::queries::code_security::Refusal;

use codeyard_database::queries::code_security::TOGGLES;

use codeyard_database::queries::code_security::apply;

use codeyard_database::queries::code_security::RepositorySettings;

use codeyard_database::queries::code_security::Configuration;
