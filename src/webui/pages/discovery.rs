//! Request-loaded discovery views. Models contain only fields intended for display.
use crate::webui::{
    components::{compact_date, count_label, DraftSelect, FieldError, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

const INPUT: &str = "w-full border border-border bg-background text-sm text-foreground";
const BUTTON: &str = "btn btn-primary";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepositoryCard {
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub is_private: bool,
    pub stars: i32,
    pub forks: i32,
    pub default_branch: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IssueCard {
    pub owner: String,
    pub repo: String,
    pub number: i32,
    pub title: String,
    pub state: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserCard {
    pub username: String,
    pub avatar_url: Option<String>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
}

/// A repository another user shares with the viewer, and the viewer's role on it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedRepositoryCard {
    pub repository: RepositoryCard,
    pub role: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HomeModel {
    pub username: Option<String>,
    pub avatar_url: Option<String>,
    pub repositories: Vec<RepositoryCard>,
    pub shared: Vec<SharedRepositoryCard>,
    pub trending: Vec<RepositoryCard>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExploreModel {
    pub sort: String,
    pub limit: usize,
    pub query: String,
    pub loaded_count: usize,
    pub repositories: Vec<RepositoryCard>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchModel {
    pub query: String,
    pub tab: String,
    pub repositories: Vec<RepositoryCard>,
    pub issues: Vec<IssueCard>,
    pub users: Vec<UserCard>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewRepositoryForm {
    pub name: String,
    pub description: String,
    pub is_private: bool,
    /// Untrusted selection; empty means the current actor's personal namespace.
    #[serde(default)]
    pub owner_org_id: String,
}

/// A refused submission. A `field` puts the message beside that control; without one it
/// is shown above the form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormError {
    pub field: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryCreationOrganization {
    pub id: i64,
    pub slug: String,
}

/// Display choices only, never a client-side write permit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryCreationChoices {
    pub organizations: Vec<RepositoryCreationOrganization>,
    pub after: i64,
    pub limit: i64,
    pub next_after: Option<i64>,
}

/// Organization choices shown per page when the address names no limit.
const DEFAULT_CHOICE_LIMIT: i64 = 50;

impl Default for RepositoryCreationChoices {
    fn default() -> Self {
        Self {
            organizations: Vec::new(),
            after: 0,
            limit: DEFAULT_CHOICE_LIMIT,
            next_after: None,
        }
    }
}

impl RepositoryCreationChoices {
    /// The create page showing choices after `after`. Default paging stays out of the address.
    pub fn page_url(&self, after: i64) -> String {
        let mut query = Vec::new();
        if after > 0 {
            query.push(format!("org_after={after}"));
        }
        if self.limit != DEFAULT_CHOICE_LIMIT {
            query.push(format!("org_limit={}", self.limit));
        }
        if query.is_empty() {
            "/new".to_owned()
        } else {
            format!("/new?{}", query.join("&"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    Home(HomeModel),
    Explore(ExploreModel),
    Search(SearchModel),
    NewRepository {
        owner: String,
        csrf_token: String,
        #[serde(default)]
        choices: RepositoryCreationChoices,
        form: NewRepositoryForm,
        error: Option<FormError>,
    },
    Demo {
        no_nav: bool,
    },
}

pub fn render(page: Page) -> Element {
    match page {
        Page::Home(model) => home(model),
        Page::Explore(model) => explore(model),
        Page::Search(model) => search(model),
        Page::NewRepository {
            owner,
            csrf_token,
            choices,
            form,
            error,
        } => new_repository(owner, csrf_token, choices, form, error),
        Page::Demo { no_nav } => demo(no_nav),
    }
}

pub fn repository_url(owner: &str, name: &str) -> String {
    format!(
        "/{}/{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name)
    )
}

pub fn search_tab_url(query: &str, tab: &str) -> String {
    format!(
        "/search?q={}&type={}",
        crate::webui::encode_component(query),
        crate::webui::encode_component(tab)
    )
}

fn repository_card(repo: &RepositoryCard) -> Element {
    let href = repository_url(&repo.owner, &repo.name);
    rsx! {
        article { class: "border-b border-border px-4 py-4 sm:px-5",
            div { class: "flex flex-wrap items-center gap-2",
                h2 { class: "min-w-0 text-base font-semibold",
                    NavLink { href, class: "text-primary hover:underline", "{repo.owner}/{repo.name}" }
                }
                span { class: "rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground", if repo.is_private { "Private" } else { "Public" } }
            }
            if let Some(description) = &repo.description {
                p { class: "mt-1 text-sm text-muted-foreground", "{description}" }
            }
            div { class: "mt-2 flex flex-wrap items-center gap-4 text-xs text-muted-foreground",
                span { "{repo.stars} stars" }
                span { "{repo.forks} forks" }
                span { "{repo.default_branch}" }
                time { datetime: repo.updated_at.clone(), "Updated {compact_date(&repo.updated_at)}" }
            }
        }
    }
}

pub fn home(model: HomeModel) -> Element {
    if model.username.is_none() {
        return rsx! {
            section { class: "mx-auto flex min-h-[calc(100dvh-56px)] w-full max-w-5xl items-center px-4 py-12 sm:px-6 lg:px-8",
                div { class: "w-full max-w-3xl space-y-6",
                    img { src: "/assets/logo.webp", alt: "Codeyard logo", width: "512", height: "512", class: "h-auto w-24 max-w-full" }
                    p { class: "text-xs font-semibold uppercase tracking-[0.2em] text-muted-foreground", "Codeyard" }
                    h1 { class: "text-4xl font-semibold tracking-tight text-foreground sm:text-5xl", "Git hosting, written in Rust." }
                    p { class: "max-w-2xl text-base leading-relaxed text-muted-foreground", "Self-hosted source control with repositories, pull requests, and issues in a clean, familiar interface." }
                    div { class: "flex flex-wrap items-center gap-3 pt-2",
                        NavLink { href: "/register", class: BUTTON, "Sign up" }
                        NavLink { href: "/login", class: "focus-ring inline-flex items-center rounded-md border border-border bg-background px-5 py-2.5 text-sm font-medium", "Sign in" }
                    }
                }
            }
        };
    }
    rsx! {
        div { class: "page-shell discovery-dashboard",
            section { class: "discovery-dashboard-main", aria_labelledby: "dashboard-repositories-title",
                header { class: "discovery-dashboard-header",
                    h1 { id: "dashboard-repositories-title",
                        "Your repositories"
                        span { class: "discovery-repository-count", aria_label: count_label(model.repositories.len(), "repository", "repositories"), "{model.repositories.len()}" }
                    }
                    NavLink { href: "/new", class: "focus-ring discovery-new-repository", "New repository" }
                }
                if model.repositories.is_empty() {
                    div { class: "discovery-dashboard-empty",
                        h2 { "No repositories yet" }
                        p {
                            NavLink { href: "/new", class: "focus-ring discovery-link", "Create a repository" }
                            " to host your code, or "
                            NavLink { href: "/explore", class: "focus-ring discovery-link", "explore public repositories" }
                            "."
                        }
                    }
                } else {
                    ul { class: "discovery-repositories",
                        for repo in &model.repositories {
                            li { class: "discovery-repository-row",
                                div { class: "discovery-repository-heading",
                                    h2 {
                                        NavLink { href: repository_url(&repo.owner, &repo.name), class: "focus-ring discovery-link", "{repo.owner}/{repo.name}" }
                                    }
                                    span { class: "discovery-repository-visibility", if repo.is_private { "Private" } else { "Public" } }
                                }
                                if let Some(description) = &repo.description {
                                    p { class: "discovery-repository-description", "{description}" }
                                }
                                div { class: "discovery-repository-meta",
                                    span { "{repo.stars} stars" }
                                    span { "{repo.forks} forks" }
                                    span { "Branch {repo.default_branch}" }
                                    time { datetime: repo.updated_at.clone(), "Updated {compact_date(&repo.updated_at)}" }
                                }
                            }
                        }
                    }
                }
                if !model.shared.is_empty() {
                    section { class: "discovery-shared", aria_labelledby: "dashboard-shared-title",
                        header { class: "discovery-dashboard-header",
                            h2 { id: "dashboard-shared-title",
                                "Shared with you"
                                span { class: "discovery-repository-count", aria_label: "{model.shared.len()} shared repositories", "{model.shared.len()}" }
                            }
                        }
                        ul { class: "discovery-repositories",
                            for shared in &model.shared {
                                li { class: "discovery-repository-row",
                                    div { class: "discovery-repository-heading",
                                        h3 {
                                            NavLink { href: repository_url(&shared.repository.owner, &shared.repository.name), class: "focus-ring discovery-link", "{shared.repository.owner}/{shared.repository.name}" }
                                        }
                                        span { class: "discovery-repository-visibility", "{shared.role}" }
                                    }
                                    if let Some(description) = &shared.repository.description {
                                        p { class: "discovery-repository-description", "{description}" }
                                    }
                                    div { class: "discovery-repository-meta",
                                        span { "{shared.repository.stars} stars" }
                                        span { "{shared.repository.forks} forks" }
                                        span { "Branch {shared.repository.default_branch}" }
                                        time { datetime: shared.repository.updated_at.clone(), "Updated {compact_date(&shared.repository.updated_at)}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            aside { class: "discovery-dashboard-trending", aria_labelledby: "dashboard-trending-title",
                header { class: "discovery-trending-header",
                    h2 { id: "dashboard-trending-title", "Trending" }
                    NavLink { href: "/explore?sort=stars", class: "focus-ring discovery-link", "Explore" }
                }
                p { class: "discovery-trending-scope", "Most starred of the 100 newest public repositories." }
                if model.trending.is_empty() {
                    p { class: "discovery-trending-empty", "Nothing trending from other people yet." }
                } else {
                    ul { class: "discovery-trending-list",
                        for repo in &model.trending {
                            li {
                                h3 {
                                    NavLink { href: repository_url(&repo.owner, &repo.name), class: "focus-ring discovery-link", "{repo.owner}/{repo.name}" }
                                }
                                if let Some(description) = &repo.description {
                                    p { class: "discovery-trending-description", "{description}" }
                                }
                                div { class: "discovery-repository-meta",
                                    span { "{repo.stars} stars" }
                                    span { "{repo.forks} forks" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn explore(model: ExploreModel) -> Element {
    let clear_href = format!(
        "/explore?sort={}&limit={}",
        crate::webui::encode_component(&model.sort),
        model.limit
    );
    let count = count_label(model.repositories.len(), "repository", "repositories");
    let heading = if model.query.is_empty() {
        "Public repositories".to_owned()
    } else {
        format!("{count} matching “{}”", model.query)
    };
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-6 space-y-1",
                h1 { class: "text-2xl font-semibold tracking-tight", "Explore repositories" }
                p { class: "text-sm text-muted-foreground", "Discover public projects across codeyard." }
            }
            form { method: "get", action: "/explore", class: "filter-toolbar explore-filters panel mb-4 p-4",
                label { r#for: "explore-q", class: "sr-only", "Search repositories" }
                input { id: "explore-q", r#type: "search", name: "q", value: model.query.clone(), maxlength: 256, class: "filter-search", placeholder: "Search repositories" }
                label { class: "filter-control",
                    span { "Sort:" }
                    DraftSelect { name: "sort", initial_value: model.sort.to_string(), options: vec![
                        SelectOption::new("recent", "Newest"),
                        SelectOption::new("stars", "Most stars"),
                    ] }
                }
                label { class: "filter-control",
                    span { "Show:" }
                    DraftSelect { name: "limit", initial_value: model.limit.to_string(), options: [10usize, 25, 50, 100]
                        .into_iter()
                        .map(|size| SelectOption::new(size.to_string(), size.to_string()))
                        .collect::<Vec<_>>() }
                }
                button { r#type: "submit", class: "btn", "Apply" }
            }
            div { class: "mb-3 flex items-center justify-between gap-3 text-sm",
                h2 { class: "font-semibold", "{heading}" }
                if model.query.is_empty() { p { role: "status", class: "text-xs text-muted-foreground", "{count}" } }
            }
            section { class: "panel overflow-hidden", aria_label: "{heading}",
                if model.repositories.is_empty() {
                    div { class: "px-4 py-14 text-center",
                        if model.query.is_empty() {
                            h2 { class: "text-base font-semibold", "No repositories yet" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Public repositories will appear here." }
                        } else {
                            h2 { class: "text-base font-semibold", "No repositories match “{model.query}”" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Try a different name or description." }
                            NavLink { href: clear_href, class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Clear search" }
                        }
                    }
                }
                for repo in &model.repositories { {repository_card(repo)} }
            }
        }
    }
}

pub fn search(model: SearchModel) -> Element {
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-5",
                h1 { class: "text-2xl font-semibold tracking-tight", if model.query.trim().is_empty() { "Search Codeyard" } else { "Search results" } }
                if !model.query.trim().is_empty() { p { class: "mt-1 text-sm text-muted-foreground", "Results for \"{model.query}\"" } }
            }
            form { action: "/search", method: "get", class: "mb-6 flex items-end gap-3",
                div { class: "flex-1",
                    label { r#for: "search-q", class: "sr-only", "Search repositories, issues, and people" }
                    input { id: "search-q", name: "q", "value": model.query.clone(), maxlength: 256, class: INPUT, placeholder: "Search repositories, issues, and people" }
                }
                input { r#type: "hidden", name: "type", value: model.tab.clone() }
                button { r#type: "submit", class: BUTTON, "Search" }
            }
            div { class: "grid grid-cols-1 gap-6 md:grid-cols-[220px_1fr]",
                nav { class: "flex flex-col gap-2", aria_label: "Search result type",
                    for (tab, label, count) in [("repos", "Repositories", model.repositories.len()), ("issues", "Issues", model.issues.len()), ("users", "People", model.users.len())] {
                        NavLink { href: search_tab_url(&model.query, tab), class: if model.tab == tab { "focus-ring flex items-center justify-between rounded-md bg-accent px-3 py-2 text-sm font-medium" } else { "focus-ring flex items-center justify-between rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-accent" },
                            span { "{label}" }
                            span { class: "rounded-full bg-muted px-2 py-0.5 text-xs", "{count}" }
                        }
                    }
                }
                section { class: "min-w-0", aria_label: "Search results",
                    if model.query.trim().is_empty() {
                        div { class: "panel px-4 py-14 text-center",
                            h2 { class: "text-base font-semibold", "Find something on Codeyard" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Search public repositories, issues, and people." }
                        }
                    } else if model.tab == "issues" {
                        div { class: "panel overflow-hidden",
                            if model.issues.is_empty() { div { class: "px-4 py-14 text-center", h2 { class: "font-semibold", "No issues found" } } }
                            for issue in &model.issues {
                                article { class: "border-b border-border px-4 py-4",
                                    div { class: "flex flex-wrap items-center gap-2",
                                        span { class: "rounded-full border border-border px-2 py-0.5 text-xs", "{issue.state}" }
                                        NavLink { href: format!("{}/issues/{}", repository_url(&issue.owner, &issue.repo), issue.number), class: "text-base font-semibold text-primary hover:underline", "{issue.title}" }
                                    }
                                    p { class: "mt-2 text-xs text-muted-foreground", "{issue.owner}/{issue.repo} #{issue.number} · Updated {compact_date(&issue.updated_at)}" }
                                }
                            }
                        }
                    } else if model.tab == "users" {
                        div { class: "panel overflow-hidden",
                            if model.users.is_empty() { div { class: "px-4 py-14 text-center", h2 { class: "font-semibold", "No people found" } } }
                            for user in &model.users {
                                article { class: "flex items-start gap-3 border-b border-border px-4 py-4",
                                    if let Some(avatar) = &user.avatar_url {
                                        img { src: avatar.clone(), alt: "{user.username}'s avatar", class: "h-10 w-10 rounded-full border border-border object-cover" }
                                    } else {
                                        div { class: "flex h-10 w-10 items-center justify-center rounded-full border border-border bg-muted", aria_hidden: "true", "@" }
                                    }
                                    div { class: "min-w-0",
                                        h2 { class: "text-sm font-semibold", NavLink { href: format!("/{}", crate::webui::encode_component(&user.username)), class: "text-primary hover:underline", "{user.display_name.as_deref().unwrap_or(&user.username)}" } }
                                        p { class: "text-xs text-muted-foreground", "@{user.username}" }
                                        if let Some(bio) = &user.bio { p { class: "mt-1 text-sm text-muted-foreground", "{bio}" } }
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "panel overflow-hidden",
                            if model.repositories.is_empty() { div { class: "px-4 py-14 text-center",
                                h2 { class: "font-semibold", "No repositories found for \"{model.query}\"" }
                                p { class: "mt-2 text-sm text-muted-foreground", "Try a broader search term." }
                            } }
                            for repo in &model.repositories { {repository_card(repo)} }
                        }
                    }
                }
            }
        }
    }
}

pub fn new_repository(
    owner: String,
    csrf_token: String,
    choices: RepositoryCreationChoices,
    form: NewRepositoryForm,
    error: Option<FormError>,
) -> Element {
    rsx! { NewRepositoryView { owner, csrf_token, choices, form, error } }
}

// Browser feedback never replaces server validation or authorization.
pub fn valid_repository_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !matches!(name, "." | "..")
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[component]
fn NewRepositoryView(
    owner: String,
    csrf_token: String,
    choices: RepositoryCreationChoices,
    form: NewRepositoryForm,
    error: Option<FormError>,
) -> Element {
    let mut name = use_signal(|| form.name.clone());
    let mut is_private = use_signal(|| form.is_private);
    let mut submitting = use_signal(|| false);
    let invalid_name = !name().is_empty() && !valid_repository_name(&name());
    let (banner, name_error) = match error {
        Some(FormError { field, message }) if field.as_deref() == Some("name") => {
            (None, Some(message))
        }
        Some(FormError { message, .. }) => (Some(message), None),
        None => (None, None),
    };
    // A server message about the name applies only until the name is edited.
    let name_error = name_error.filter(|_| name() == form.name);
    let name_marked = invalid_name || name_error.is_some();
    let name_message = name_error.or_else(|| {
        invalid_name.then(|| "Check the repository name: use 1–100 bytes, letters, numbers, hyphens, underscores or dots; not . or .." .to_owned())
    });
    // The rules hint is replaced by the error while one is shown, so it names only the element
    // that is rendered.
    let name_described_by = if name_marked {
        "repo-name-error"
    } else {
        "repo-name-help"
    };
    let unavailable_selection = !form.owner_org_id.is_empty()
        && !choices
            .organizations
            .iter()
            .any(|org| org.id.to_string() == form.owner_org_id);
    let action = choices.page_url(choices.after);
    let mut owner_options = vec![SelectOption::new("", format!("{owner} (personal)"))];
    owner_options.extend(
        choices.organizations.iter().map(|org| {
            SelectOption::new(org.id.to_string(), format!("{} (organization)", org.slug))
        }),
    );
    if unavailable_selection {
        owner_options.push(SelectOption::new(
            form.owner_org_id.clone(),
            "Selected organization unavailable on this page",
        ));
    }
    rsx! {
        div { class: "container mx-auto max-w-2xl p-4 py-8",
            h1 { class: "mb-2 text-2xl font-bold", "Create a new repository" }
            p { class: "mb-8 text-muted-foreground", "A repository contains all project files, including the revision history." }
            if let Some(message) = banner { div { role: "alert", class: "mb-6 rounded-md border border-red-500/30 bg-red-500/10 p-4 text-red-400", "{message}" } }
            // Browser constraint checks would show only a native bubble that is not part of
            // the page. The server validates the same rules and marks the field itself.
            form { method: "post", action, novalidate: true, onsubmit: move |_| submitting.set(true), class: "space-y-6",
                input { r#type: "hidden", name: "csrf_token", value: csrf_token }
                div { class: "space-y-6 rounded-md border border-border bg-card p-6",
                    div {
                        label { r#for: "repo-owner", class: "mb-2 block text-sm font-medium", "Owner" }
                        DraftSelect { id: Some("repo-owner".to_owned()), name: "owner_org_id", class: INPUT, initial_value: form.owner_org_id.clone(), options: owner_options }
                        p { class: "mt-1 text-xs text-muted-foreground", "Only organizations where you are an owner or admin are listed. Permission is checked again when creating." }
                        if unavailable_selection {
                            p { class: "mt-1 text-xs text-muted-foreground", "This page cannot confirm the selected organization. Choose a listed owner or reload the page." }
                        }
                        nav { class: "mt-2 flex gap-3 text-sm", aria_label: "Repository owner choices",
                            if choices.after > 0 { NavLink { href: choices.page_url(0), class: "text-primary hover:underline", "First organization choices" } }
                            if let Some(after) = choices.next_after { NavLink { href: choices.page_url(after), class: "text-primary hover:underline", "Next organization choices" } }
                        }
                    }
                    div {
                        label { r#for: "repo-name", class: "mb-2 block text-sm font-medium", "Repository name (required)" }
                        input { id: "repo-name", name: "name", value: name(), oninput: move |event| name.set(event.value()), aria_required: "true", maxlength: 100, aria_invalid: name_marked.to_string(), aria_describedby: name_described_by, class: INPUT, placeholder: "my-awesome-project",
                            // Keep a name typed before hydration, as DraftInput does.
                            onmounted: move |event| {
                                #[cfg(target_arch = "wasm32")]
                                { use wasm_bindgen::JsCast;
                                  if let Some(element) = event.data().downcast::<web_sys::Element>() {
                                      if let Some(input) = element.dyn_ref::<web_sys::HtmlInputElement>() { name.set(input.value()); }
                                  } }
                                #[cfg(not(target_arch = "wasm32"))]
                                let _ = event;
                            } }
                        if let Some(message) = name_message { FieldError { id: "repo-name-error".to_owned(), message } }
                        if !name_marked { p { id: "repo-name-help", class: "mt-1 text-xs text-muted-foreground", "1–100 bytes: letters, numbers, hyphens, underscores and dots. Not . or .." } }
                    }
                    div {
                        label { r#for: "repo-description", class: "mb-2 block text-sm font-medium", "Description (optional)" }
                        input { id: "repo-description", name: "description", "value": form.description, maxlength: 2000, class: INPUT, placeholder: "Short description of your repository" }
                    }
                    fieldset { class: "collaboration-option-group",
                        legend { class: "text-sm font-medium", "Visibility" }
                        label { class: if !is_private() { "flex cursor-pointer items-start gap-3 rounded-md border border-success-emphasis p-3 ring-1 ring-success-emphasis" } else { "flex cursor-pointer items-start gap-3 rounded-md border border-border p-3" },
                            input { r#type: "radio", name: "visibility", value: "public", checked: !is_private(), onchange: move |_| is_private.set(false), class: "mt-1" }
                            div { p { class: "font-medium", "Public" } p { class: "text-sm text-muted-foreground", "Anyone who can reach this server can see this repository." } }
                        }
                        label { class: if is_private() { "flex cursor-pointer items-start gap-3 rounded-md border border-success-emphasis p-3 ring-1 ring-success-emphasis" } else { "flex cursor-pointer items-start gap-3 rounded-md border border-border p-3" },
                            input { r#type: "radio", name: "visibility", value: "private", checked: is_private(), onchange: move |_| is_private.set(true), class: "mt-1" }
                            div { p { class: "font-medium", "Private" } p { class: "text-sm text-muted-foreground", "You choose who can see and commit to this repository." } }
                        }
                    }
                }
                div { class: "flex justify-end gap-3",
                    NavLink { href: "/", class: "btn btn-secondary", "Cancel" }
                    button { r#type: "submit", disabled: submitting(), class: BUTTON, if submitting() { "Creating…" } else { "Create repository" } }
                }
            }
        }
    }
}

pub fn demo(no_nav: bool) -> Element {
    rsx! {
        div { class: "mx-auto max-w-2xl space-y-4 p-8",
            img { src: "/assets/logo.webp", alt: "Codeyard logo", width: "512", height: "512", class: "h-auto w-16" }
            h1 { class: "text-3xl font-semibold tracking-tight", if no_nav { "Focused view" } else { "Codeyard interface" } }
            p { class: "text-muted-foreground", if no_nav { "A distraction-free page for focused tasks." } else { "A familiar workspace for repositories, collaboration, and automation." } }
            NavLink { href: if no_nav { "/demo" } else { "/demo/no-nav" }, class: BUTTON, if no_nav { "Back to interface" } else { "Open focused view" } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_encode_segments_and_preserve_search_queries() {
        assert_eq!(repository_url("a/b", "x?#"), "/a%2Fb/x%3F%23");
        assert_eq!(
            search_tab_url("rust & wasm+#", "issues"),
            "/search?q=rust%20%26%20wasm%2B%23&type=issues"
        );
    }

    #[test]
    fn hydration_page_models_round_trip_only_presentation_data() {
        let page = Page::Home(HomeModel {
            username: Some("alice".into()),
            ..Default::default()
        });
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), page);
        for private_field in [
            "password_hash",
            "email",
            "disk_path",
            "is_admin",
            "two_factor_enabled",
        ] {
            assert!(!json.contains(private_field));
        }
    }

    #[test]
    fn hydrated_name_feedback_rejects_bad_names_without_becoming_server_authority() {
        for name in ["", ".", "..", "../demo", "a/b", "a\\b", "bad name"] {
            assert!(!valid_repository_name(name));
        }
        assert!(!valid_repository_name(&"a".repeat(101)));
        assert!(valid_repository_name("demo-repo_1.0"));
        assert!(valid_repository_name("répo"));
    }

    #[test]
    fn explore_with_no_match_offers_a_clear_search_link_that_keeps_the_scope() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "stars".into(),
                limit: 25,
                query: "python".into(),
                loaded_count: 0,
                repositories: vec![],
            }))
        });
        assert!(html.contains("No repositories match"));
        assert!(html.contains("python"));
        assert!(html.contains("Clear search"));
        assert!(html.contains("sort=stars"));
        assert!(html.contains("limit=25"));
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "recent".into(),
                limit: 50,
                query: String::new(),
                loaded_count: 0,
                repositories: vec![],
            }))
        });
        assert!(!html.contains("Clear search"));
    }

    #[test]
    fn search_renders_escaped_content_real_links_and_loaded_not_total_counts() {
        let html = dioxus_ssr::render_element(search(SearchModel {
            query: "<script>alert(1)</script>&".into(),
            tab: "issues".into(),
            issues: vec![IssueCard {
                owner: "alice".into(),
                repo: "demo".into(),
                number: 4,
                title: "<img src=x onerror=alert(1)>".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        assert!(html.contains("&#60;script&#62;"));
        assert!(html.contains("&#60;img"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("/alice/demo/issues/4"));
        assert!(html.contains("Issues"));
        assert!(html.contains(">1</span>"));
        assert!(!html.contains("loaded"));
    }

    #[test]
    fn empty_repository_search_names_the_query_and_suggests_a_broader_term() {
        let html = dioxus_ssr::render_element(search(SearchModel {
            query: "zzz".into(),
            tab: "repos".into(),
            ..Default::default()
        }));
        assert!(html.contains("No repositories found for"));
        assert!(html.contains("zzz"));
        assert!(html.contains("Try a broader search term."));
    }

    #[test]
    fn creation_models_round_trip_and_missing_fields_remain_personal() {
        let page = Page::NewRepository {
            owner: "alice".into(),
            csrf_token: "fixture-csrf".into(),
            choices: RepositoryCreationChoices {
                organizations: vec![RepositoryCreationOrganization {
                    id: 4,
                    slug: "native_org".into(),
                }],
                after: 2,
                limit: 1,
                next_after: Some(4),
            },
            form: NewRepositoryForm {
                owner_org_id: "4".into(),
                is_private: true,
                ..Default::default()
            },
            error: None,
        };
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), page);
        for field in [
            "billing_email",
            "storage_operation",
            "storage_actor_id",
            "disk_path",
            "governance_suspended",
        ] {
            assert!(!json.contains(field));
        }
        let legacy = r#"{"NewRepository":{"owner":"alice","csrf_token":"fixture-csrf","form":{"name":"demo","description":"draft","is_private":false},"error":null}}"#;
        match serde_json::from_str::<Page>(legacy).unwrap() {
            Page::NewRepository { choices, form, .. } => {
                assert_eq!(choices, RepositoryCreationChoices::default());
                assert!(form.owner_org_id.is_empty());
            }
            _ => panic!("legacy creation model"),
        }
    }

    #[test]
    fn a_refused_name_is_marked_beside_its_field_and_keeps_the_draft() {
        let html = dioxus_ssr::render_element(new_repository(
            "alice".into(),
            "csrf-test".into(),
            RepositoryCreationChoices::default(),
            NewRepositoryForm {
                name: String::new(),
                description: "kept draft".into(),
                is_private: false,
                owner_org_id: String::new(),
            },
            Some(FormError {
                field: Some("name".into()),
                message: "Repository name must be 1–100 bytes.".into(),
            }),
        ));
        assert!(html.contains("novalidate"));
        assert!(!html.contains(" required="));
        assert!(html.contains("aria-required=\"true\""));
        assert!(!html.contains("role=\"alert\""));
        assert!(html.contains("id=\"repo-name-error\""));
        assert!(html.contains("Repository name must be 1–100 bytes."));
        assert!(html.contains("aria-invalid=\"true\""));
        assert!(html.contains("aria-describedby=\"repo-name-error\""));
        assert!(!html.contains("repo-name-help"));
        assert!(html.contains("kept draft"));
    }

    #[test]
    fn creation_choice_renderer_escapes_slugs_and_preserves_native_selection_and_cursor() {
        let html = dioxus_ssr::render_element(new_repository(
            "alice".into(),
            "fixture-csrf".into(),
            RepositoryCreationChoices {
                organizations: vec![RepositoryCreationOrganization {
                    id: 4,
                    slug: "<script>org</script>".into(),
                }],
                after: 2,
                limit: 1,
                next_after: Some(4),
            },
            NewRepositoryForm {
                owner_org_id: "4".into(),
                ..Default::default()
            },
            None,
        ));
        assert!(!html.contains("<script>org</script>"));
        assert!(html.contains("&#60;script&#62;org&#60;/script&#62;"));
        assert!(!html.contains("Selected organization unavailable"));
        assert!(html.contains("/new?org_after=4&#38;org_limit=1"));
        assert!(html.contains("/new?org_limit=1"));
        assert!(html.contains("action=\"/new?org_after=2&#38;org_limit=1\""));
        let option = html
            .split("<option")
            .find(|option| option.split('>').next().unwrap().contains("value=\"4\""))
            .unwrap();
        assert!(option.split('>').next().unwrap().contains("selected"));
    }

    #[test]
    fn creation_form_preserves_values_csrf_and_visibility_without_inert_controls() {
        let html = dioxus_ssr::render_element(new_repository(
            "alice".into(),
            "csrf-test".into(),
            RepositoryCreationChoices::default(),
            NewRepositoryForm {
                name: "bad/name\"".into(),
                description: "<script>".into(),
                is_private: true,
                owner_org_id: "99".into(),
            },
            Some(FormError {
                field: None,
                message: "Repository already exists".into(),
            }),
        ));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/new\""));
        assert!(html.contains("name=\"owner_org_id\""));
        assert!(html.contains("Selected organization unavailable on this page"));
        assert!(html.contains("Only organizations where you are an owner or admin are listed."));
        assert!(!html.contains("governance"));
        assert!(html.contains("Cancel"));
        assert!(html.contains("value=\"99\""));
        assert!(html.contains("name=\"csrf_token\""));
        assert!(html.contains("csrf-test"));
        assert!(html.contains("bad/name&quot;") || html.contains("bad/name&#34;"));
        assert!(html.contains("&#60;script&#62;"));
        let private_input = html
            .split("<input")
            .find(|input| {
                input
                    .split('>')
                    .next()
                    .is_some_and(|attributes| attributes.contains("value=\"private\""))
            })
            .unwrap();
        assert!(private_input.split('>').next().unwrap().contains("checked"));
        assert!(html.contains("Repository already exists"));
        assert!(!html.contains("disabled"));
    }

    #[test]
    fn explore_preserves_filters_and_labels_the_bounded_scope() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "stars".into(),
                limit: 10,
                query: "Rust & Git".into(),
                loaded_count: 10,
                repositories: vec![RepositoryCard {
                    owner: "alice".into(),
                    name: "demo".into(),
                    description: Some("Rust & Git".into()),
                    ..Default::default()
                }],
            }))
        });
        assert!(html.contains("method=\"get\""));
        assert!(html.contains("action=\"/explore\""));
        assert!(html.contains("Rust &#38; Git"));
        assert!(html.contains("1 repository matching “Rust &#38; Git”"));
        assert!(!html.contains("Public repositories"));
        // The page size is a select, not a number field with spinner arrows.
        assert!(html.contains("name=\"limit\""));
        assert!(html.contains(">25</option>"));
        assert!(!html.contains("type=\"number\""));
        assert!(!html.contains("platform-wide"));
        assert!(html.contains("/alice/demo"));
    }

    #[test]
    fn demo_routes_have_real_bidirectional_links_without_developer_scaffolding() {
        let html = dioxus_ssr::render_element(demo(false));
        assert!(html.contains("/demo/no-nav"));
        assert!(html.contains("Codeyard interface"));
        assert!(!html.contains("transition library"));
        let html = dioxus_ssr::render_element(demo(true));
        assert!(html.contains("href=\"/demo\""));
        assert!(html.contains("Focused view"));
    }

    #[test]
    fn home_has_real_content_before_any_client_runtime() {
        let html = dioxus_ssr::render_element(home(HomeModel::default()));
        assert!(html.contains("Git hosting, written in Rust."));
        assert!(html.contains("src=\"/assets/logo.webp\""));
        assert!(html.contains("width=\"512\""));
        assert!(html.contains("height=\"512\""));
        assert!(html.contains("href=\"/register\""));
        assert!(!html.contains("Loading"));
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            repositories: vec![RepositoryCard {
                owner: "alice".into(),
                name: "private".into(),
                is_private: true,
                ..Default::default()
            }],
            ..Default::default()
        }));
        assert!(html.contains("Your repositories"));
        assert!(html.contains("/alice/private"));
        assert!(html.contains("Private"));
    }

    #[test]
    fn dashboard_places_all_owned_repositories_before_secondary_trending() {
        let repositories = (1..=12)
            .map(|number| RepositoryCard {
                owner: "alice".into(),
                name: format!("owned-{number}"),
                description: Some(format!("Project {number}")),
                is_private: number == 1,
                stars: number,
                forks: 2,
                default_branch: "main".into(),
                updated_at: "2026-10-02".into(),
            })
            .collect();
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            repositories,
            trending: vec![RepositoryCard {
                owner: "bob".into(),
                name: "popular".into(),
                stars: 20,
                ..Default::default()
            }],
            ..Default::default()
        }));
        let primary_heading = html.find("id=\"dashboard-repositories-title\"").unwrap();
        let first_owned = html.find("href=\"/alice/owned-1\"").unwrap();
        let last_owned = html.find("href=\"/alice/owned-12\"").unwrap();
        let sidebar = html.find("<aside").unwrap();
        let trending = html.find("href=\"/bob/popular\"").unwrap();
        assert!(primary_heading < first_owned);
        assert!(first_owned < last_owned);
        assert!(last_owned < sidebar);
        assert!(sidebar < trending);
        assert!(html[..sidebar].contains("<h1"));
        assert!(html[..sidebar].contains("href=\"/new\""));
        assert!(html.contains("aria-label=\"12 repositories\""));
        assert_eq!(
            html.matches("class=\"discovery-repository-row\"").count(),
            12
        );
        for number in 1..=12 {
            assert!(html.contains(&format!("href=\"/alice/owned-{number}\"")));
            assert!(html.contains(&format!("Project {number}")));
        }
        assert!(html.contains("Private"));
        assert!(html.contains("Public"));
        assert!(html.contains("Branch main"));
        assert!(html.contains("Updated Oct 2, 2026"));
        assert!(html.contains("2 forks"));
        assert!(html[sidebar..].contains("href=\"/explore?sort=stars\""));
        assert!(html.contains("Most starred of the 100 newest public repositories."));
        for rejected_panel in ["Welcome back", "Personal account", "recent activity"] {
            assert!(!html.contains(rejected_panel));
        }
    }

    #[test]
    fn trending_is_not_hidden_below_the_side_panel_width() {
        // Below 960px the trending panel stacks under the repository list instead of disappearing.
        let css = include_str!("../input.css");
        assert!(
            !css.contains(".discovery-dashboard .discovery-dashboard-trending {\n\tdisplay: none;"),
            "trending is hidden at every width"
        );
    }

    #[test]
    fn dashboard_empty_states_offer_real_actions_without_fabricated_rows() {
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            ..Default::default()
        }));
        let sidebar = html.find("<aside").unwrap();
        assert!(html[..sidebar].contains("Your repositories"));
        assert!(html[..sidebar].contains("aria-label=\"0 repositories\""));
        assert!(html[..sidebar].contains("No repositories yet"));
        assert!(html[..sidebar].contains("href=\"/new\""));
        assert!(html[..sidebar].contains("Create a repository"));
        assert!(html[..sidebar].contains("href=\"/explore\""));
        assert!(html[sidebar..].contains("Nothing trending from other people yet."));
        assert!(!html.contains("class=\"discovery-repository-row\""));
        assert!(!html.contains("class=\"discovery-trending-list\""));
        assert!(!html.contains("Loading"));
    }

    #[test]
    fn dashboard_keeps_owned_empty_state_primary_when_trending_has_data() {
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            trending: vec![RepositoryCard {
                owner: "bob".into(),
                name: "popular".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        let empty = html.find("No repositories yet").unwrap();
        let trending = html.find("href=\"/bob/popular\"").unwrap();
        assert!(empty < trending);
        assert!(html.contains("href=\"/new\""));
        assert!(!html.contains("Nothing trending from other people yet."));
        assert!(!html.contains("class=\"discovery-repository-row\""));
    }

    #[test]
    fn dashboard_shares_collaborator_repositories_with_a_role_only_when_there_are_some() {
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            ..Default::default()
        }));
        assert!(!html.contains("Shared with you"));
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            shared: vec![SharedRepositoryCard {
                repository: RepositoryCard {
                    owner: "bob".into(),
                    name: "tool".into(),
                    description: Some("Shared tooling".into()),
                    ..Default::default()
                },
                role: "Write".into(),
            }],
            ..Default::default()
        }));
        let shared = html.find("Shared with you").unwrap();
        let sidebar = html.find("<aside").unwrap();
        assert!(shared < sidebar);
        assert!(html[shared..sidebar].contains("href=\"/bob/tool\""));
        assert!(html[shared..sidebar].contains("Shared tooling"));
        assert!(html[shared..sidebar].contains("Write"));
        assert!(html[..shared].contains("No repositories yet"));
    }

    #[test]
    fn dashboard_escapes_repository_fields_and_encodes_links_in_both_columns() {
        let malicious = RepositoryCard {
            owner: "a/b".into(),
            name: "x\"<script>".into(),
            description: Some("<img src=x onerror=alert(1)>".into()),
            default_branch: "<script>alert(1)</script>".into(),
            updated_at: "2026-10-02\" onmouseover=\"alert(1)".into(),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            repositories: vec![malicious.clone()],
            trending: vec![malicious],
            ..Default::default()
        }));
        assert_eq!(html.matches("href=\"/a%2Fb/x%22%3Cscript%3E\"").count(), 2);
        assert!(html.contains("&#60;script&#62;"));
        assert!(html.contains("&#60;img"));
        assert!(html.contains("&#34;") || html.contains("&quot;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains(" onmouseover=\"alert(1)\""));
    }
}
