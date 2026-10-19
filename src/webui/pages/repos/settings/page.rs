use crate::webui::pages::repos::chrome::{chrome, csrf, icon, repository_id, RepoChrome};
use crate::webui::{
    components::{
        confirm_dialog,
        deploy_keys::{DeployKeyPanel, DeployKeySection},
        webhooks::WebhookPanel,
        webhooks::WebhookSection,
        Avatar, DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption,
    },
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Collaborator {
    pub username: String,
    pub access: String,
}
/// A branch protection rule as the settings page shows and edits it. A saved rule and a refused
/// submission have the same shape, so a refused form comes back with what was typed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BranchRule {
    pub pattern: String,
    pub require_pull_request: bool,
    /// Text, so that an unfinished number comes back as it was typed.
    pub required_approving_reviews: String,
    pub dismiss_stale_reviews: bool,
    pub require_status_checks: bool,
    pub required_status_checks: Vec<String>,
    pub require_up_to_date_branch: bool,
    pub require_linear_history: bool,
    pub require_conversation_resolution: bool,
    pub require_code_owner_review: bool,
    pub enforce_admins: bool,
    pub allow_force_pushes: bool,
    pub allow_deletions: bool,
    pub restrict_pushes: bool,
    pub push_allowances: Vec<String>,
    pub force_push_allowances: Vec<String>,
}
/// A new rule starts with the settings the JSON API defaults to.
impl Default for BranchRule {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            require_pull_request: true,
            required_approving_reviews: "0".to_owned(),
            dismiss_stale_reviews: true,
            require_status_checks: false,
            required_status_checks: Vec::new(),
            require_up_to_date_branch: false,
            require_linear_history: false,
            require_conversation_resolution: false,
            require_code_owner_review: false,
            enforce_admins: true,
            allow_force_pushes: false,
            allow_deletions: false,
            restrict_pushes: false,
            push_allowances: Vec::new(),
            force_push_allowances: Vec::new(),
        }
    }
}
/// A refused rule form: the rule as it was submitted, and why it was refused.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuleDraft {
    pub rule: BranchRule,
    pub message: String,
}
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// The section to show, as `settings_tab` names it. Anything else shows General.
    pub tab: String,
    /// The signed-in username, offered first in a rule's force push list.
    pub viewer: String,
    /// Other users with write access, offered as the rest of that list.
    pub force_push_suggestions: Vec<Collaborator>,
    pub collaborators: Vec<Collaborator>,
    /// Every branch, for the default-branch picker, and the branch the repository opens on.
    pub branches: Vec<String>,
    pub default_branch: String,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub branch_rules: Vec<BranchRule>,
    pub rule_draft: Option<RuleDraft>,
    pub error: Option<String>,
    /// The General save succeeded. Its redirect carries `saved=general`.
    pub saved: bool,
    /// The webhooks section, on the webhooks tab only. Boxed: it is large, and most pages lack it.
    pub webhooks: Option<Box<WebhookPanel>>,
    /// The deploy keys section, on the deploy-keys tab only.
    pub deploy_keys: Option<Box<DeployKeyPanel>>,
}

pub fn settings(repo: &RepoChrome, settings: &Settings) -> Element {
    rsx! { for id in [repo.id] { SettingsView { key: "{id}", repo: repo.clone(), settings: settings.clone() } } }
}

/// The sections of the repository settings page, one tab each. An unknown name shows General, and
/// Collaborators is offered only where this page manages members.
pub fn settings_tab(name: &str, can_manage_collaborators: bool) -> &'static str {
    match name {
        "merging" => "merging",
        "branches" => "branches",
        "webhooks" => "webhooks",
        "deploy-keys" => "deploy-keys",
        "collaborators" if can_manage_collaborators => "collaborators",
        "danger" => "danger",
        _ => "general",
    }
}

/// The section a repository settings form posts from. A saved or refused form returns there.
/// Other repository forms return to their own pages, so they have no section.
pub fn settings_section(action: &str) -> Option<&'static str> {
    Some(match action {
        "settings" | "rename" | "default-branch" => "general",
        "merge-methods" => "merging",
        "protection-save" | "protection-delete" => "branches",
        "webhook-save" | "webhook-delete" | "webhook-ping" | "webhook-redeliver" => "webhooks",
        "deploy-key-add" | "deploy-key-delete" => "deploy-keys",
        "collaborator" | "remove-collaborator" => "collaborators",
        "archive" | "unarchive" | "delete" => "danger",
        _ => return None,
    })
}

/// One section link in the settings nav. The current section carries `aria-current="page"`.
fn section_link(repo: &RepoChrome, section: &str, current: &str, label: &str) -> Element {
    let class = if section == "danger" {
        "repo-settings-danger-link"
    } else {
        ""
    };
    let current = (section == current).then(|| "page".to_owned());
    rsx! { NavLink { href: format!("{}/settings?tab={section}", repo.base()), class: class.to_owned(), aria_current: current, "{label}" } }
}

/// The sections as one menu, for a phone: the tab row has no room there, and a wrapped row looks broken.
fn section_picker(repo: &RepoChrome, current: &str) -> Element {
    let mut sections = vec![
        ("general", "General"),
        ("merging", "Merging"),
        ("branches", "Branch protection"),
        ("webhooks", "Webhooks"),
        ("deploy-keys", "Deploy keys"),
    ];
    if repo.can_manage_collaborators {
        sections.push(("collaborators", "Collaborators"));
    }
    sections.push(("labels", "Labels"));
    sections.push(("danger", "Danger zone"));
    let label = sections
        .iter()
        .find(|(id, _)| *id == current)
        .map_or("Settings", |(_, label)| *label);
    rsx! {
        details { class: "native-menu repo-menu repo-settings-picker",
            summary { class: "btn btn-secondary", aria_label: "Settings sections", span { "{label}" } {icon("chevron")} }
            div { class: "repo-menu-panel",
                for (id, text) in sections {
                    NavLink {
                        class: "repo-menu-item".to_owned(),
                        href: if id == "labels" { format!("{}/labels", repo.base()) } else { format!("{}/settings?tab={id}", repo.base()) },
                        aria_current: (id == current).then(|| "page".to_owned()),
                        "{text}"
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsView(repo: RepoChrome, settings: Settings) -> Element {
    let tab = settings_tab(&settings.tab, repo.can_manage_collaborators);
    chrome(
        &repo,
        rsx! {
            section { class: "repo-settings-page",
                header { class: "repo-page-heading", div { h2 { "Repository settings" } p { "Manage this repository's details and access." } } }
                if settings.saved { div { class: "repo-alert repo-alert-success", role: "status", "Settings saved." } }
                div { class: "repo-settings-layout",
                    nav { class: "repo-settings-nav", aria_label: "Settings sections",
                        {section_link(&repo, "general", tab, "General")}
                        {section_link(&repo, "merging", tab, "Merging")}
                        {section_link(&repo, "branches", tab, "Branch protection")}
                        {section_link(&repo, "webhooks", tab, "Webhooks")}
                        {section_link(&repo, "deploy-keys", tab, "Deploy keys")}
                        if repo.can_manage_collaborators { {section_link(&repo, "collaborators", tab, "Collaborators")} }
                        NavLink { href: format!("{}/labels", repo.base()), "Labels" }
                        {section_link(&repo, "danger", tab, "Danger zone")}
                    }
                    {section_picker(&repo, tab)}
                    div { class: "repo-settings-content",
                        // A refused save shows here, above the section whose form it came from.
                        if let Some(error) = &settings.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { "Settings were not saved" } p { "{error}" } } }
                        if tab == "webhooks" {
                            if let Some(panel) = &settings.webhooks { WebhookSection { panel: panel.as_ref().clone() } }
                        }
                        if tab == "deploy-keys" {
                            if let Some(panel) = &settings.deploy_keys { DeployKeySection { panel: panel.as_ref().clone() } }
                        }
                        if tab == "general" {
                        form { class: if repo.is_private { "repo-settings-section panel repo-visibility-private" } else { "repo-settings-section panel repo-visibility-public" }, method: "post", action: repo.action("settings"),
                            {csrf(&repo.csrf_token)}
                            {repository_id(repo.id)}
                            header { class: "repo-section-heading", div { h3 { "General" } p { "Basic repository information and visibility." } } }
                            div { class: "repo-form-body",
                                label { class: "repo-field", span { "Description" } DraftTextarea { name: "description", rows: 3, maxlength: Some(10000), initial_value: repo.description.clone() } small { "A short summary shown on the repository page." } }
                                fieldset { class: "collaboration-option-group",
                                    legend { "Visibility" }
                                    label { class: "repo-visibility-option flex cursor-pointer items-start gap-3 rounded-md border border-border p-3",
                                        input { r#type: "radio", name: "visibility", value: "public", checked: !repo.is_private, class: "mt-1" }
                                        div { p { class: "font-medium", "Public" } p { class: "text-sm text-muted-foreground", "Anyone who can reach this server can see this repository." } }
                                    }
                                    label { class: "repo-visibility-option flex cursor-pointer items-start gap-3 rounded-md border border-border p-3",
                                        input { r#type: "radio", name: "visibility", value: "private", checked: repo.is_private, class: "mt-1" }
                                        div { p { class: "font-medium", "Private" } p { class: "text-sm text-muted-foreground", "You choose who can see and commit to this repository." } }
                                    }
                                }
                                // Shown by CSS only when the choice differs from the saved visibility. The server
                                // asks for it then, so the box is not `required`: a hidden required box would
                                // block a description-only save.
                                label { class: "repo-check-label repo-visibility-confirm", input { r#type: "checkbox", name: "confirm", value: "yes" } span { "I understand that changing visibility affects who can view this repository." } }
                                div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Save changes" } }
                            }
                        }
                        }
                        if tab == "general" {
                        section { class: "repo-settings-section panel",
                            header { class: "repo-section-heading", div { h3 { "Default branch" } p { "Repository pages and new clones open on this branch." } } }
                            if settings.branches.is_empty() {
                                p { class: "repo-list-empty", "The repository has no branches yet. Push one to choose a default." }
                            } else {
                                form { method: "post", action: repo.action("default-branch"), class: "repo-form-body",
                                    {csrf(&repo.csrf_token)}
                                    {repository_id(repo.id)}
                                    label { class: "repo-field", span { "Default branch" } DraftSelect { name: "branch", initial_value: settings.default_branch.clone(), options: settings.branches.iter().map(|branch| SelectOption { value: branch.clone(), label: branch.clone() }).collect::<Vec<_>>() } }
                                    div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Save default branch" } }
                                }
                            }
                        }
                        }
                        if tab == "general" {
                        section { class: "repo-settings-section panel",
                            header { class: "repo-section-heading", div { h3 { "Wiki" } p { if repo.wiki_enabled { "The wiki is on. Turning it off hides its pages; they are kept." } else { "The wiki is off. Turning it on shows its pages again." } } } }
                            form { method: "post", action: format!("{}/wiki/_settings", repo.base()), class: "repo-form-body",
                                {csrf(&repo.csrf_token)}
                                input { r#type: "hidden", name: "enabled", value: if repo.wiki_enabled { "0" } else { "1" } }
                                div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", if repo.wiki_enabled { "Turn wiki off" } else { "Turn wiki on" } } }
                            }
                        }
                        }
                        if tab == "merging" {
                        section { class: "repo-settings-section panel",
                            header { class: "repo-section-heading", div { h3 { "Pull request merging" } p { "Choose which merge methods pull requests may use. At least one stays allowed." } } }
                            form { method: "post", action: repo.action("merge-methods"), class: "repo-form-body",
                                {csrf(&repo.csrf_token)}
                                {repository_id(repo.id)}
                                {merge_method_checks(settings.allow_merge_commit, settings.allow_squash_merge, settings.allow_rebase_merge)}
                                div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Save changes" } }
                            }
                        }
                        }
                        if tab == "branches" {
                            {branch_protection(&repo, &settings)}
                        }
                        if tab == "general" {
                        section { class: "repo-settings-section panel",
                            header { class: "repo-section-heading", div { h3 { "Repository name" } p { "Rename this repository without changing its history." } } }
                            form { method: "post", action: repo.action("rename"), class: "repo-form-body",
                                {csrf(&repo.csrf_token)}
                                {repository_id(repo.id)}
                                label { class: "repo-field", span { "Repository name" } DraftInput { name: "name", required: true, maxlength: Some(100), initial_value: repo.name.clone() } }
                                label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Confirm repository rename" }
                                div { class: "repo-form-actions", button { class: "btn btn-secondary", r#type: "submit", "Rename repository" } }
                            }
                        }
                        }
                        if tab == "collaborators" {
                            section { class: "repo-settings-section panel",
                                header { class: "repo-section-heading",
                                    div {
                                        div { class: "title-with-count", h3 { "Collaborators" } span { class: "count-badge", "{settings.collaborators.len()}" } }
                                        p { "Give people access to this repository." }
                                    }
                                }
                                div { class: "repo-settings-list",
                                    if settings.collaborators.is_empty() { p { class: "repo-list-empty", "No collaborators have been added." } }
                                    for collaborator in &settings.collaborators {
                                        div { key: "{collaborator.username}", class: "repo-settings-row",
                                            div { class: "repo-settings-row-main", Avatar { name: collaborator.username.clone(), size: 32 } div { strong { "{collaborator.username}" } p { "{collaborator.access} access" } } }
                                            details { class: "native-menu repo-delete-menu",
                                                summary { class: "btn btn-secondary", "Remove" span { class: "repo-sr-only", " {collaborator.username}" } }
                                                {confirm_dialog(None, &format!("Remove {} from this repository", collaborator.username), &format!("{}/settings?tab=collaborators", repo.base()), &repo.action("remove-collaborator"), rsx! {
                                                    {csrf(&repo.csrf_token)}
                                                    {repository_id(repo.id)}
                                                    input { r#type: "hidden", name: "username", value: collaborator.username.clone() }
                                                    strong { "Remove {collaborator.username}?" }
                                                    label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " They lose access to this repository." }
                                                }, "Remove")}
                                            }
                                        }
                                    }
                                }
                                form { method: "post", action: repo.action("collaborator"), class: "repo-form-body repo-add-row",
                                    {csrf(&repo.csrf_token)}
                                    {repository_id(repo.id)}
                                    label { class: "repo-field", span { "Username" } DraftInput { name: "username", required: true, initial_value: String::new(), placeholder: "Username" } }
                                    label { class: "repo-field", span { "Access" } DraftSelect { name: "access", initial_value: "write".to_owned(), options: vec![
                                        SelectOption { value: "read".into(), label: "Read".into() },
                                        SelectOption { value: "write".into(), label: "Write".into() },
                                        SelectOption { value: "admin".into(), label: "Admin".into() }
                                    ] } }
                                    button { r#type: "submit", class: "btn btn-primary", "Add collaborator" }
                                }
                            }
                        }
                        if tab == "danger" {
                        section { class: "repo-settings-section repo-danger-zone panel",
                            header { class: "repo-section-heading", div { h3 { "Danger zone" } p { "Destructive actions require confirmation." } } }
                            div { class: "repo-danger-action",
                                if repo.is_archived {
                                    div { strong { "Unarchive this repository" } p { "Accept pushes, issues, pull requests and comments again." } }
                                    details { class: "native-menu repo-delete-menu",
                                        summary { class: "btn btn-secondary", "Unarchive" }
                                        {confirm_dialog(None, "Unarchive this repository", &format!("{}/settings?tab=danger", repo.base()), &repo.action("unarchive"), rsx! {
                                            {csrf(&repo.csrf_token)}
                                            {repository_id(repo.id)}
                                            strong { "Unarchive this repository?" }
                                            label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " Allow changes to this repository again" }
                                        }, "Unarchive repository")}
                                    }
                                } else {
                                    div { strong { "Archive this repository" } p { "Make it read-only for everyone. Pushes, issues, pull requests and comments are refused until it is unarchived." } }
                                    details { class: "native-menu repo-delete-menu",
                                        summary { class: "btn btn-secondary", "Archive" }
                                        {confirm_dialog(None, "Archive this repository", &format!("{}/settings?tab=danger", repo.base()), &repo.action("archive"), rsx! {
                                            {csrf(&repo.csrf_token)}
                                            {repository_id(repo.id)}
                                            strong { "Archive this repository?" }
                                            label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " Nobody can change this repository until it is unarchived" }
                                        }, "Archive repository")}
                                    }
                                }
                            }
                            div { class: "repo-danger-action",
                                div { strong { "Transfer ownership" } p { "Move this repository to another account or organization you can create repositories for." } }
                                details { class: "native-menu repo-delete-menu",
                                    summary { class: "btn btn-destructive", "Transfer ownership" }
                                    {confirm_dialog(None, "Transfer this repository", &format!("{}/settings?tab=danger", repo.base()), &repo.action("transfer"), rsx! {
                                        {csrf(&repo.csrf_token)}
                                        {repository_id(repo.id)}
                                        strong { "Transfer this repository?" }
                                        label { class: "repo-field", span { "New owner" } DraftInput { name: "new_owner", required: true, initial_value: String::new(), placeholder: "Username or organization" } }
                                        p { "Type " strong { "{repo.name}" } " to continue." }
                                        label { class: "repo-field", span { "Repository name" } DraftInput { name: "confirmation", required: true, initial_value: String::new() } }
                                    }, "Transfer repository")}
                                }
                            }
                            div { class: "repo-danger-action",
                                div { strong { "Delete this repository" } p { "Permanently remove the repository and its data." } }
                                details { class: "native-menu repo-delete-menu",
                                    summary { class: "btn btn-destructive", "Delete repository" }
                                    {confirm_dialog(None, "Delete this repository", &format!("{}/settings?tab=danger", repo.base()), &repo.action("delete"), rsx! {
                                        {csrf(&repo.csrf_token)}
                                        {repository_id(repo.id)}
                                        strong { "Delete this repository?" }
                                        p { "This action cannot be undone. Type " strong { "{repo.owner}/{repo.name}" } " to continue." }
                                        label { class: "repo-field", span { "Repository name" } DraftInput { name: "confirmation", required: true, initial_value: String::new() } }
                                    }, "Permanently delete repository")}
                                }
                            }
                        }
                        }
                    }
                }
            }
        },
    )
}

fn field_label(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

/// The merge-method checkboxes. The settings page and a rejected save of them both render these.
fn merge_method_checks(merge: bool, squash: bool, rebase: bool) -> Element {
    rsx! {
        label { class: "repo-check-label", DraftCheckbox { name: "allow_merge_commit", initial_checked: merge } span { "Allow merge commits" } }
        label { class: "repo-check-label", DraftCheckbox { name: "allow_squash_merge", initial_checked: squash } span { "Allow squash merging" } }
        label { class: "repo-check-label", DraftCheckbox { name: "allow_rebase_merge", initial_checked: rebase } span { "Allow rebase merging" } }
    }
}

/// The branch protection section: each saved rule as a card that opens to its edit form, and the
/// form that adds one. A refused save is shown in the form that sent it, with the values it sent.
fn branch_protection(repo: &RepoChrome, settings: &Settings) -> Element {
    let draft = settings.rule_draft.as_ref();
    let draft_is_listed = draft.is_some_and(|draft| {
        settings
            .branch_rules
            .iter()
            .any(|rule| rule.pattern == draft.rule.pattern)
    });
    let (add_rule, add_message) = match draft {
        Some(draft) if !draft_is_listed => (draft.rule.clone(), Some(draft.message.clone())),
        _ => (BranchRule::default(), None),
    };
    let listed: Vec<(BranchRule, Option<RuleDraft>)> = settings
        .branch_rules
        .iter()
        .map(|rule| {
            let refused = draft
                .filter(|draft| draft.rule.pattern == rule.pattern)
                .cloned();
            (rule.clone(), refused)
        })
        .collect();
    rsx! {
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading",
                div {
                    div { class: "title-with-count", h3 { "Branch protection" } span { class: "count-badge", "{settings.branch_rules.len()}" } }
                    p { "Require pull requests, approvals and checks before a branch changes." }
                }
            }
            div { class: "repo-settings-list",
                if settings.branch_rules.is_empty() { p { class: "repo-list-empty", "No branch protection rules have been added." } }
                for (rule, refused) in listed {
                    BranchRuleRow { key: "{rule.pattern}", repo: repo.clone(), rule: rule.clone(), refused, viewer: settings.viewer.clone(), suggestions: settings.force_push_suggestions.clone() }
                }
            }
            header { class: "repo-section-heading repo-rule-add-heading",
                div { h4 { "Add a rule" } p { "Protect branches that match a name or pattern." } }
            }
            BranchRuleForm { repo: repo.clone(), rule: add_rule, editing: false, message: add_message, viewer: settings.viewer.clone(), suggestions: settings.force_push_suggestions.clone() }
        }
    }
}

/// One saved rule as a card. The header names the pattern and lists what it enforces. The body
/// holds the edit form, which opens by itself when a save of this rule was refused, and the delete
/// row at its foot.
#[component]
fn BranchRuleRow(
    repo: RepoChrome,
    rule: BranchRule,
    refused: Option<RuleDraft>,
    viewer: String,
    suggestions: Vec<Collaborator>,
) -> Element {
    let chips = rule_summary(&rule);
    let (shown, message) = match refused {
        Some(refused) => (refused.rule, Some(refused.message)),
        None => (rule.clone(), None),
    };
    let open = message.is_some();
    rsx! {
        details { class: "repo-rule", open: open,
            summary { class: "repo-rule-header",
                span { class: "repo-rule-title",
                    code { class: "repo-rule-pattern", "{rule.pattern}" }
                    span { class: "repo-rule-chips",
                        for chip in chips { span { class: "repo-label-neutral repo-rule-chip", "{chip}" } }
                    }
                }
                span { class: "btn btn-secondary btn-sm repo-rule-edit", "Edit" {icon("chevron")} }
            }
            div { class: "repo-rule-body",
                BranchRuleForm { repo: repo.clone(), rule: shown, editing: true, message, viewer, suggestions }
                div { class: "repo-rule-danger",
                    div { strong { "Delete this rule" } p { "Matching branches stop following this rule." } }
                    details { class: "native-menu repo-delete-menu",
                        summary { class: "btn btn-destructive", "Delete rule" }
                        {confirm_dialog(None, &format!("Delete rule {}", rule.pattern), &format!("{}/settings?tab=branches", repo.base()), &repo.action("protection-delete"), rsx! {
                            {csrf(&repo.csrf_token)}
                            {repository_id(repo.id)}
                            input { r#type: "hidden", name: "pattern", value: rule.pattern.clone() }
                            strong { "Delete rule {rule.pattern}?" }
                            label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " I understand" }
                        }, "Delete rule")}
                    }
                }
            }
        }
    }
}

/// The fields of a branch protection rule, for the add form and for each rule's edit form. An
/// edit keeps the pattern, which names the rule, so the pattern is posted but not typed again.
/// A refused save shows its message at the top, where a card that opens to it is read first.
#[component]
fn BranchRuleForm(
    repo: RepoChrome,
    rule: BranchRule,
    editing: bool,
    message: Option<String>,
    viewer: String,
    suggestions: Vec<Collaborator>,
) -> Element {
    let submit = if editing { "Save rule" } else { "Add rule" };
    rsx! {
        form { method: "post", action: repo.action("protection-save"), class: "repo-form-body repo-rule-form",
            {csrf(&repo.csrf_token)}
            {repository_id(repo.id)}
            if let Some(message) = &message { div { class: "repo-alert repo-alert-error", role: "alert", p { "{message}" } } }
            if editing {
                input { r#type: "hidden", name: "pattern", value: rule.pattern.clone() }
            } else {
                label { class: "repo-field", span { "Branch name pattern" }
                    DraftInput { name: "pattern", required: true, initial_value: rule.pattern.clone(), placeholder: "main or release/**" }
                    small { "A branch name. In a pattern, * matches within one part of the name, ** also across slashes, and ? one character." }
                }
            }
            fieldset { class: "repo-rule-group",
                legend { "Pull requests" }
                label { class: "repo-option", DraftCheckbox { name: "require_pull_request", initial_checked: rule.require_pull_request }
                    span { class: "repo-option-text", "Require a pull request before merging" small { "Changes must come in through a pull request. Direct pushes, editor commits and API writes are refused." } }
                }
                div { class: "repo-rule-nested",
                    label { class: "repo-field", span { "Require approvals" }
                        DraftInput { name: "required_approving_reviews", input_type: "text".to_owned(), inputmode: Some("numeric".to_owned()), required: true, initial_value: rule.required_approving_reviews.clone(), class: "repo-rule-count" }
                        small { "Approving reviews a pull request needs before it can merge. 0 requires none." }
                    }
                    label { class: "repo-option", DraftCheckbox { name: "dismiss_stale_reviews", initial_checked: rule.dismiss_stale_reviews }
                        span { class: "repo-option-text", "Dismiss stale approvals when new commits are pushed" small { "An approval counts only for the commit it names, so a new push needs a new approval." } }
                    }
                    label { class: "repo-option", DraftCheckbox { name: "require_conversation_resolution", initial_checked: rule.require_conversation_resolution }
                        span { class: "repo-option-text", "Require conversation resolution before merging" small { "A merge is refused while a conversation on a file is unresolved." } }
                    }
                    label { class: "repo-option", DraftCheckbox { name: "require_code_owner_review", initial_checked: rule.require_code_owner_review }
                        span { class: "repo-option-text", "Require review from Code Owners" small { "A merge waits for an approval from an owner of each changed file that has owners in CODEOWNERS." } }
                    }
                }
            }
            fieldset { class: "repo-rule-group",
                legend { "Status checks" }
                label { class: "repo-option", DraftCheckbox { name: "require_status_checks", initial_checked: rule.require_status_checks }
                    span { class: "repo-option-text", "Require status checks to pass before merging" small { "Merges wait until the named checks pass on the pull request's head commit." } }
                }
                div { class: "repo-rule-nested",
                    label { class: "repo-field", span { "Required status checks" }
                        DraftInput { name: "required_status_checks", initial_value: join_status_checks(&rule.required_status_checks) }
                        small { "Check names separated by commas. Put a name that contains a comma in double quotes." }
                    }
                    label { class: "repo-option", DraftCheckbox { name: "require_up_to_date_branch", initial_checked: rule.require_up_to_date_branch }
                        span { class: "repo-option-text", "Require branches to be up to date before merging" small { "The pull request must contain the base branch's latest commit before it can merge." } }
                    }
                }
            }
            fieldset { class: "repo-rule-group",
                legend { "History" }
                label { class: "repo-option", DraftCheckbox { name: "require_linear_history", initial_checked: rule.require_linear_history }
                    span { class: "repo-option-text", "Require linear history" small { "Pushes may not add merge commits to this branch." } }
                }
            }
            fieldset { class: "repo-rule-group",
                legend { "Pushes" }
                label { class: "repo-option", DraftCheckbox { name: "block_force_pushes", initial_checked: !rule.allow_force_pushes }
                    span { class: "repo-option-text", "Do not allow force pushes" small { "Pushes that rewrite the branch's history are refused, administrators and the repository owner included." } }
                }
                div { class: "repo-rule-nested",
                    {force_push_picker(&rule, &viewer, &suggestions)}
                }
                label { class: "repo-option", DraftCheckbox { name: "block_deletions", initial_checked: !rule.allow_deletions }
                    span { class: "repo-option-text", "Do not allow deletions" small { "Pushes that delete the branch are refused. The default branch can never be deleted." } }
                }
                label { class: "repo-option", DraftCheckbox { name: "restrict_pushes", initial_checked: rule.restrict_pushes }
                    span { class: "repo-option-text", "Restrict who can push" small { "Only the users listed below may push or merge. Listing a user does not skip the pull request rule." } }
                }
                div { class: "repo-rule-nested",
                    label { class: "repo-field", span { "Users allowed to push" }
                        DraftInput { name: "push_allowances", initial_value: rule.push_allowances.join(", ") }
                        small { "Usernames separated by commas or spaces. Each needs write access to this repository." }
                    }
                }
            }
            fieldset { class: "repo-rule-group",
                legend { "Administrators" }
                label { class: "repo-option", DraftCheckbox { name: "enforce_admins", initial_checked: rule.enforce_admins }
                    span { class: "repo-option-text", "Include administrators" small { "These rules apply to repository administrators too. When this is off, administrators skip them for direct writes and merges." } }
                }
            }
            div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "{submit}" } }
        }
    }
}

/// Who may force push past a rule. The viewer comes first, then the suggested people, then anyone
/// the rule lists who is no longer suggested, so saving never drops them silently. Names typed in
/// the extra field are added on save.
fn force_push_picker(rule: &BranchRule, viewer: &str, suggestions: &[Collaborator]) -> Element {
    let listed = |name: &str| {
        rule.force_push_allowances
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
    };
    let suggested = |name: &str| {
        suggestions
            .iter()
            .any(|person| person.username.eq_ignore_ascii_case(name))
    };
    let unsuggested: Vec<&String> = rule
        .force_push_allowances
        .iter()
        .filter(|name| !name.eq_ignore_ascii_case(viewer) && !suggested(name))
        .collect();
    rsx! {
        div { class: "repo-rule-nested",
            div { class: "repo-field",
                span { "Users who may force push anyway" }
                if !viewer.is_empty() {
                    label { class: "repo-check-label",
                        DraftCheckbox { name: "force_push_allowances", submission_value: viewer.to_owned(), initial_checked: listed(viewer) }
                        span { "You (@{viewer})" }
                    }
                }
                for person in suggestions {
                    label { class: "repo-check-label",
                        DraftCheckbox { name: "force_push_allowances", submission_value: person.username.clone(), initial_checked: listed(&person.username) }
                        span { "{person.username} " small { "{person.access}" } }
                    }
                }
                for name in unsuggested {
                    label { class: "repo-check-label",
                        DraftCheckbox { name: "force_push_allowances", submission_value: name.clone(), initial_checked: true }
                        span { "{name}" }
                    }
                }
            }
            label { class: "repo-field", span { "Someone else (usernames, separated by commas)" }
                DraftInput { name: "force_push_extra", initial_value: String::new() }
                small { "Leave empty to block everyone else. Each name needs write access to this repository." }
            }
        }
    }
}

/// Status check names as one text field: entries separated by ", ". A name that contains a comma
/// or a quote is wrapped in double quotes with each quote doubled, so any name survives a round trip.
pub fn join_status_checks(checks: &[String]) -> String {
    checks
        .iter()
        .map(|check| {
            if check.contains([',', '"']) {
                format!("\"{}\"", check.replace('"', "\"\""))
            } else {
                check.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The inverse of `join_status_checks`. Entries are trimmed and blank ones dropped. A quoted entry
/// runs to its closing quote, and text between that quote and the next comma is ignored. A quote
/// that never closes takes the rest of the text literally, so malformed input never fails.
pub fn split_status_checks(text: &str) -> Vec<String> {
    let mut checks = Vec::new();
    let mut rest = text;
    loop {
        let entry = rest.trim_start();
        let (name, after) = match entry.strip_prefix('"') {
            Some(quoted) => {
                let (name, after) = read_quoted(quoted);
                (name, after.find(',').map_or("", |comma| &after[comma..]))
            }
            None => {
                let end = entry.find(',').unwrap_or(entry.len());
                (entry[..end].trim().to_owned(), &entry[end..])
            }
        };
        if !name.is_empty() {
            checks.push(name);
        }
        match after.strip_prefix(',') {
            Some(next) => rest = next,
            None => return checks,
        }
    }
}

/// Reads a quoted name after its opening quote. `""` is one quote, and a single quote closes the
/// name. Returns the name and the text after the closing quote.
fn read_quoted(text: &str) -> (String, &str) {
    let mut name = String::new();
    let mut rest = text;
    while let Some(quote) = rest.find('"') {
        name.push_str(&rest[..quote]);
        let after = &rest[quote + 1..];
        match after.strip_prefix('"') {
            Some(doubled) => {
                name.push('"');
                rest = doubled;
            }
            None => return (name, after),
        }
    }
    name.push_str(rest);
    (name, "")
}

/// What a rule enforces, one chip per setting. Settings that are off are not mentioned.
fn rule_summary(rule: &BranchRule) -> Vec<String> {
    let approvals = rule
        .required_approving_reviews
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    let mut parts = Vec::new();
    if rule.require_pull_request {
        parts.push("Pull request required".to_owned());
    }
    if approvals > 0 {
        let plural = if approvals == 1 { "" } else { "s" };
        parts.push(format!("{approvals} approval{plural} required"));
    }
    if rule.require_status_checks {
        parts.push(format!(
            "Status checks: {}",
            join_status_checks(&rule.required_status_checks)
        ));
    }
    if rule.require_up_to_date_branch {
        parts.push("Up to date before merging".to_owned());
    }
    if rule.require_conversation_resolution {
        parts.push("Conversations resolved before merging".to_owned());
    }
    if rule.require_code_owner_review {
        parts.push("Code owner review before merging".to_owned());
    }
    if rule.require_linear_history {
        parts.push("Linear history".to_owned());
    }
    if !rule.allow_force_pushes {
        parts.push("No force pushes".to_owned());
        if !rule.force_push_allowances.is_empty() {
            parts.push(format!(
                "Force pushes allowed for {}",
                rule.force_push_allowances.join(", ")
            ));
        }
    }
    if !rule.allow_deletions {
        parts.push("No deletions".to_owned());
    }
    if rule.restrict_pushes {
        let users = if rule.push_allowances.is_empty() {
            "no one".to_owned()
        } else {
            rule.push_allowances.join(", ")
        };
        parts.push(format!("Pushes limited to {users}"));
    }
    parts.push(
        if rule.enforce_admins {
            "Applies to administrators"
        } else {
            "Administrators can bypass"
        }
        .to_owned(),
    );
    parts
}

/// Failed native forms retain submitted, non-secret values so the user can correct them.
pub fn rejected_form(
    repo: &RepoChrome,
    action: &str,
    message: &str,
    values: &std::collections::HashMap<String, String>,
) -> Element {
    let mut fields: Vec<_> = values
        .iter()
        .filter(|(key, _)| key.as_str() != "csrf_token" && key.as_str() != "confirm")
        .collect();
    fields.sort_by(|a, b| a.0.cmp(b.0));
    // The merge-method values from `safe_submission` are "true" or "false".
    let checked = |name: &str| values.get(name).is_some_and(|value| value == "true");
    let back = match settings_section(action) {
        Some(section) => format!("{}/settings?tab={section}", repo.base()),
        None => repo.base(),
    };
    chrome(
        repo,
        rsx! {
            section { class: "repo-retry-page",
                div { class: "repo-alert repo-alert-error", role: "alert", strong { "We couldn't complete that action" } p { "{message}" } }
                form { method: "post", action: repo.action(action), class: "repo-settings-section panel",
                    {csrf(&repo.csrf_token)}
                    {repository_id(repo.id)}
                    header { class: "repo-section-heading", div { h2 { "Review and try again" } p { "Your entries have been kept." } } }
                    div { class: "repo-form-body",
                        if action == "merge-methods" {
                            {merge_method_checks(checked("allow_merge_commit"), checked("allow_squash_merge"), checked("allow_rebase_merge"))}
                        } else {
                            for (name, value) in fields { label { key: "{name}", class: "repo-field", span { "{field_label(name)}" } DraftTextarea { name: name.clone(), initial_value: value.clone(), rows: 2 } } }
                        }
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Confirm and retry" }
                        div { class: "repo-form-actions",
                            button { class: "btn btn-primary", r#type: "submit", "Try again" }
                            NavLink { class: "btn btn-secondary".to_owned(), href: back, "Cancel" }
                        }
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{join_status_checks, split_status_checks};

    #[test]
    fn status_check_names_round_trip_through_the_form_field() {
        let checks = [
            "build".to_owned(),
            "ci / test".to_owned(),
            "build, linux".to_owned(),
            "say \"hi\"".to_owned(),
        ];
        let text = join_status_checks(&checks);
        assert_eq!(text, r#"build, ci / test, "build, linux", "say ""hi""""#);
        assert_eq!(split_status_checks(&text), checks);
    }

    #[test]
    fn malformed_status_check_text_keeps_what_it_can() {
        // A quote that never closes takes the rest of the text, commas included.
        assert_eq!(split_status_checks("\"open, linux"), ["open, linux"]);
        // Blank entries are dropped and the text around each name is trimmed.
        assert_eq!(split_status_checks(" , build ,, \"\" ,"), ["build"]);
        assert!(split_status_checks("").is_empty());
    }
}
