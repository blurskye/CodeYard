use super::{FormError, RepositoryCreationChoices};
use crate::{
    components::{DraftCheckbox, DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationForm {
    pub name: String,
    pub source_url: String,
    pub is_private: bool,
    pub owner_org_id: String,
    pub repository_id: Option<i64>,
    pub action: String,
    pub status: Option<String>,
}

#[component]
pub fn MigrationView(
    owner: String,
    csrf_token: String,
    choices: RepositoryCreationChoices,
    form: MigrationForm,
    error: Option<FormError>,
) -> Element {
    let retry = form.repository_id.is_some();
    let running = form.status.as_deref() == Some("importing");
    let mut options = vec![SelectOption::new("", format!("{owner} (personal)"))];
    options.extend(
        choices
            .organizations
            .iter()
            .map(|org| SelectOption::new(org.id.to_string(), org.slug.clone())),
    );
    if !form.owner_org_id.is_empty()
        && !options
            .iter()
            .any(|option| option.value == form.owner_org_id)
    {
        options.push(SelectOption::new(
            form.owner_org_id.clone(),
            "Selected organization (unavailable)",
        ));
    }
    rsx! {
        section { class: "container mx-auto max-w-2xl p-4 py-8",
            h1 { class: "mb-2 text-2xl font-bold", if retry { "Repository migration" } else { "New migration" } }
            p { class: "mb-6 text-muted-foreground", "Import Git history from GitHub, GitLab, Gitea or Forgejo using an HTTPS clone URL. Issues, releases and other forge metadata are not imported yet." }
            if let Some(error) = error { p { role: "alert", class: "mb-6 rounded-md border border-border p-4", "{error.message}" } }
            if let Some(status) = &form.status { p { role: "status", class: "mb-4", "Import status: {status}" } }
            form { method: "post", action: form.action, class: "space-y-6",
                {crate::pages::fields::csrf(&csrf_token)}
                if let Some(id) = form.repository_id { input { r#type: "hidden", name: "repository_id", value: id } }
                if !retry {
                    div {
                        label { r#for: "migration-owner", class: "mb-2 block text-sm font-medium", "Owner" }
                        DraftSelect { id: Some("migration-owner".to_owned()), name: "owner_org_id", initial_value: form.owner_org_id, options, class: "w-full border border-border bg-background text-sm text-foreground" }
                    }
                    div {
                        label { r#for: "migration-name", class: "mb-2 block text-sm font-medium", "Repository name" }
                        DraftInput { id: Some("migration-name".to_owned()), name: "name", initial_value: form.name, maxlength: Some(100), required: true, class: "w-full border border-border bg-background text-sm text-foreground" }
                    }
                    label { class: "repo-check-label", DraftCheckbox { name: "private", initial_checked: form.is_private } span { "Private repository" } }
                }
                div {
                    label { r#for: "migration-source", class: "mb-2 block text-sm font-medium", "Source clone URL" }
                    DraftInput { id: Some("migration-source".to_owned()), name: "source_url", initial_value: form.source_url, maxlength: Some(2048), required: true, class: "w-full border border-border bg-background text-sm text-foreground" }
                }
                div {
                    label { r#for: "migration-username", class: "mb-2 block text-sm font-medium", "Source username (optional)" }
                    input { id: "migration-username", name: "source_username", maxlength: 2048, autocomplete: "off", class: "w-full border border-border bg-background text-sm text-foreground" }
                }
                div {
                    label { r#for: "migration-token", class: "mb-2 block text-sm font-medium", "Source access token (optional)" }
                    input { id: "migration-token", r#type: "password", name: "source_token", maxlength: 2048, autocomplete: "new-password", class: "w-full border border-border bg-background text-sm text-foreground" }
                    p { class: "mt-1 text-xs text-muted-foreground", "Credentials are used only for this attempt and are never saved. Enter them again when retrying." }
                }
                div { class: "flex flex-wrap gap-3",
                    button { r#type: "submit", name: "intent", value: "start", disabled: running, class: "btn btn-primary", if retry { "Retry migration" } else { "Start migration" } }
                    if retry { button { r#type: "submit", name: "intent", value: "cancel", formnovalidate: true, class: "btn btn-secondary", "Cancel migration" } }
                    NavLink { href: "/new", class: "btn btn-secondary", "New repository" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_form_uses_native_submission_and_never_projects_credentials() {
        let form = MigrationForm {
            name: "destination".to_owned(),
            source_url: "https://example.com/owner/source.git".to_owned(),
            is_private: true,
            action: "/new/migration".to_owned(),
            ..Default::default()
        };
        let model = serde_json::to_value(&form).unwrap();
        assert!(model.get("source_token").is_none());
        assert!(model.get("source_username").is_none());
        let html = crate::tests::in_scope(|| {
            dioxus_ssr::render_element(rsx! {
                MigrationView { owner: "alice", csrf_token: "fixture-csrf", choices: RepositoryCreationChoices::default(), form, error: None }
            })
        });
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/new/migration\""));
        assert!(html.contains("name=\"csrf_token\""));
        assert!(html.contains("name=\"source_token\""));
        assert!(html.contains("type=\"password\""));
        assert!(!html.contains("source_token\" value="));
    }
}
