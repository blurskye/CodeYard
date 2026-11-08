use anyhow::ensure;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Not Found")]
    Missing,
    #[error("Organization security access required")]
    Denied,
    #[error("Invalid security configuration")]
    Invalid,
    #[error("Configuration name already exists")]
    Duplicate,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Configuration {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub settings: String,
    pub default_for_new_repos: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RepositorySettings {
    pub repo_id: i64,
    pub configuration_id: Option<i64>,
    pub security_and_analysis: String,
}

pub const TOGGLES: &[&str] = &[
    "code_security",
    "secret_protection",
    "dependency_graph",
    "dependency_graph_autosubmit_action",
    "dependabot_alerts",
    "dependabot_security_updates",
    "dependabot_delegated_alert_dismissal",
    "code_scanning_default_setup",
    "code_scanning_delegated_alert_dismissal",
    "secret_scanning",
    "secret_scanning_push_protection",
    "secret_scanning_delegated_bypass",
    "secret_scanning_validity_checks",
    "secret_scanning_non_provider_patterns",
    "secret_scanning_generic_secrets",
    "secret_scanning_delegated_alert_dismissal",
    "secret_scanning_extended_metadata",
    "private_vulnerability_reporting",
];

/// Whether a configuration turns `feature` on or off; `None` leaves it. With `enforced_only`,
/// only an enforced configuration fixes a value.
fn configured_toggle(
    configuration: &str,
    feature: &str,
    enforced_only: bool,
) -> Result<Option<bool>> {
    let value: Value = serde_json::from_str(configuration)?;
    if enforced_only && value["enforcement"] != "enforced" {
        return Ok(None);
    }
    Ok(match value[feature].as_str() {
        Some("enabled") => Some(true),
        Some("disabled") => Some(false),
        _ => None,
    })
}

/// Enforced configurations fix only the toggles they explicitly configure.
pub fn permits_toggle(configuration: &str, feature: &str, enabled: bool) -> Result<bool> {
    Ok(configured_toggle(configuration, feature, true)?.is_none_or(|fixed| fixed == enabled))
}

pub fn with_toggle_status(current: Option<&str>, feature: &str, enabled: bool) -> Result<String> {
    let mut value: Value = current
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_else(|| json!({}));
    value[feature] = json!({"status": if enabled { "enabled" } else { "disabled" }});
    Ok(value.to_string())
}

pub fn apply(current: Option<&str>, settings: &str) -> Result<String> {
    let mut value: Value = match current {
        Some(value) => serde_json::from_str(value)?,
        None => json!({}),
    };
    let settings: Value = serde_json::from_str(settings)?;
    for key in TOGGLES.iter().copied().chain(["advanced_security"]) {
        if let Some(status) = settings[key].as_str().filter(|s| *s != "not_set") {
            let status = if key == "advanced_security"
                && matches!(status, "code_security" | "secret_protection")
            {
                "enabled"
            } else {
                status
            };
            value[key] = json!({"status":status});
        }
    }
    Ok(value.to_string())
}

/// Whether a configuration turns private vulnerability reporting on or off; `None` leaves it.
pub fn private_reporting(configuration: &str, enforced_only: bool) -> Result<Option<bool>> {
    configured_toggle(
        configuration,
        "private_vulnerability_reporting",
        enforced_only,
    )
}

pub fn scanning_settings(
    applied: &str,
    previous: crate::queries::secret_scanning::Settings,
) -> Result<crate::queries::secret_scanning::Settings> {
    let value: Value = serde_json::from_str(applied)?;
    let enabled = value["secret_scanning"]["status"]
        .as_str()
        .map_or(previous.enabled, |s| s == "enabled");
    let push_protection = value["secret_scanning_push_protection"]["status"]
        .as_str()
        .map_or(previous.push_protection, |s| s == "enabled");
    ensure!(enabled || !push_protection, Refusal::Invalid);
    Ok(crate::queries::secret_scanning::Settings {
        enabled,
        push_protection,
    })
}
