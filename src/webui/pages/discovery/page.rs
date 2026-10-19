//! Request-loaded discovery views. Models contain only fields intended for display.
use crate::webui::{
    components::{
        compact_date, count_label, languages::LanguageShare, submit_form, Avatar, DraftCheckbox,
        DraftSelect, FieldError, SelectOption,
    },
    navigation::NavLink,
    pages::repos::issues::{item_icon, state_class, Kind},
    pages::users::{profile_icon, ProfileIcon},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

const INPUT: &str = "w-full border border-border bg-background text-sm text-foreground";
const BUTTON: &str = "btn btn-primary";

/// The .gitignore templates and licenses the new repository form offers. The server holds the
/// text of each one, and a backend test keeps the two lists in step.
pub const GITIGNORE_CHOICES: [&str; 11] = [
    "Rust", "Node", "Python", "Go", "Java", "C", "C++", "Dotnet", "Ruby", "Swift", "Kotlin",
];
pub const LICENSE_CHOICES: [&str; 6] = [
    "MIT",
    "Apache-2.0",
    "GPL-3.0",
    "BSD-3-Clause",
    "MPL-2.0",
    "Unlicense",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepositoryCard {
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    /// Up to five topics, shown as chips.
    #[serde(default)]
    pub topics: Vec<String>,
    pub is_private: bool,
    pub stars: i32,
    pub forks: i32,
    pub default_branch: String,
    /// The language with the most code, when the repository has a recognised one.
    #[serde(default)]
    pub primary_language: Option<LanguageShare>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IssueCard {
    pub owner: String,
    pub repo: String,
    pub number: i32,
    pub title: String,
    pub state: String,
    /// The username of the author; empty when the account is gone.
    #[serde(default)]
    pub author: String,
    /// The start of the body as plain text; empty when the body is empty.
    #[serde(default)]
    pub excerpt: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A commit whose message contains the search text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommitHit {
    pub owner: String,
    pub repo: String,
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub date: String,
}

/// A line of a file that contains the search text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeHit {
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub path: String,
    pub line_number: u32,
    pub line: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserCard {
    pub username: String,
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
    /// Open issues assigned to the viewer, and open pull requests the viewer created.
    pub assigned_issues: i64,
    pub open_pull_requests: i64,
    pub repositories: Vec<RepositoryCard>,
    pub shared: Vec<SharedRepositoryCard>,
    pub trending: Vec<RepositoryCard>,
    /// The text the repository lists are narrowed to; empty for none.
    #[serde(default)]
    pub find: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExploreModel {
    pub sort: String,
    pub query: String,
    /// The language and repository type the list is narrowed to; empty or `all` for neither.
    #[serde(default)]
    pub language: String,
    /// The topic the list is narrowed to; empty for every topic.
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub kind: String,
    /// The languages found in public repositories, for the language menu.
    #[serde(default)]
    pub languages: Vec<String>,
    /// The 1-based page shown.
    pub page: usize,
    /// Whether a page after this one has repositories.
    pub has_older: bool,
    pub repositories: Vec<RepositoryCard>,
}

/// How far a code or commit search got before its budget stopped it: `searched` of the `total`
/// repositories the viewer can read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchScan {
    pub searched: usize,
    pub total: usize,
}

/// The most a result type's count reaches. A count at this number means at least this many results.
pub const SEARCH_COUNT_CAP: usize = 1000;

/// How many results each result type has for the query, across every page, up to
/// `SEARCH_COUNT_CAP`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchCounts {
    pub repositories: usize,
    pub issues: usize,
    pub pulls: usize,
    pub users: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchModel {
    pub query: String,
    pub tab: String,
    #[serde(default)]
    pub sort: String,
    /// The 1-based page of the open result type.
    #[serde(default)]
    pub page: usize,
    /// Whether a later page of the open result type has results.
    #[serde(default)]
    pub has_older: bool,
    /// The totals of every result type, for the tab badges.
    #[serde(default)]
    pub counts: SearchCounts,
    /// Set when the budget stopped the walk over repositories before its end.
    #[serde(default)]
    pub scan: Option<SearchScan>,
    /// Repositories a code or commit search reached but could not read.
    #[serde(default)]
    pub unsearched: usize,
    /// Repositories on this page with more matches than the search shows, as `owner/name`.
    #[serde(default)]
    pub truncated: Vec<String>,
    pub repositories: Vec<RepositoryCard>,
    pub issues: Vec<IssueCard>,
    #[serde(default)]
    pub pulls: Vec<IssueCard>,
    #[serde(default)]
    pub commits: Vec<CommitHit>,
    #[serde(default)]
    pub code: Vec<CodeHit>,
    /// The text code hits contain, highlighted in their lines.
    #[serde(default)]
    pub highlight: String,
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
    /// Whether the first commit gets a README.md.
    #[serde(default)]
    pub readme: bool,
    /// The chosen .gitignore template; empty for none.
    #[serde(default)]
    pub gitignore: String,
    /// The chosen license; empty for none.
    #[serde(default)]
    pub license: String,
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

/// The viewer's starred repositories that the viewer can still read, one page of them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StarsModel {
    /// `recent`, `stars` or `name`.
    pub sort: String,
    pub query: String,
    /// The 1-based page shown.
    pub page: usize,
    /// Whether a page after this one has repositories.
    pub has_older: bool,
    pub repositories: Vec<RepositoryCard>,
    /// The token of the Unstar forms.
    #[serde(default)]
    pub csrf_token: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    Home(HomeModel),
    Explore(ExploreModel),
    Search(SearchModel),
    Stars(StarsModel),
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
        Page::Stars(model) => stars(model),
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

/// The Explore list of one topic.
pub fn topic_url(topic: &str) -> String {
    format!("/explore?topic={}", crate::webui::encode_component(topic))
}

/// A topic as a link to its Explore list.
pub(crate) fn topic_chip(topic: &str) -> Element {
    rsx! {
        NavLink { href: topic_url(topic), class: "rounded-full border border-border px-2 py-0.5 text-xs font-medium text-primary hover:underline", "{topic}" }
    }
}

/// The address of another page of the open result type, keeping its sort. Page 1 has no page
/// number.
fn search_page_href(model: &SearchModel, page: usize) -> String {
    let sort = if model.sort.is_empty() {
        String::new()
    } else {
        format!("&sort={}", crate::webui::encode_component(&model.sort))
    };
    let page = if page > 1 {
        format!("&page={page}")
    } else {
        String::new()
    };
    format!("{}{sort}{page}", search_tab_url(&model.query, &model.tab))
}

/// The Newer and Older links under a list, with the `label` of their navigation. Neither link is
/// drawn when the list has no other page.
pub fn page_links(newer: Option<String>, older: Option<String>, label: &str) -> Element {
    rsx! {
        if newer.is_some() || older.is_some() {
            nav { class: "discovery-more discovery-pager", aria_label: "{label}",
                if let Some(href) = newer { NavLink { href: href, class: "focus-ring text-primary hover:underline", "Newer" } }
                if let Some(href) = older { NavLink { href: href, class: "focus-ring text-primary hover:underline", "Older" } }
            }
        }
    }
}

/// The Newer and Older links under a page of results.
fn search_pager(model: &SearchModel) -> Element {
    let newer_href = (model.page > 1).then(|| search_page_href(model, model.page - 1));
    let older_href = model
        .has_older
        .then(|| search_page_href(model, model.page + 1));
    page_links(newer_href, older_href, "Search result pages")
}

/// One repository in a list: its name, visibility, description, topics and counts.
pub fn repository_card(repo: &RepositoryCard) -> Element {
    rsx! {
        article { class: "border-b border-border px-4 py-4 sm:px-5", {repository_summary(repo)} }
    }
}

/// The name, visibility, description, topics and facts of a repository, as the lists show them.
fn repository_summary(repo: &RepositoryCard) -> Element {
    let href = repository_url(&repo.owner, &repo.name);
    rsx! {
        div { class: "flex flex-wrap items-center gap-2",
            h2 { class: "min-w-0 text-base font-semibold",
                NavLink { href, class: "text-primary hover:underline", "{repo.owner}/{repo.name}" }
            }
            span { class: "rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground", if repo.is_private { "Private" } else { "Public" } }
        }
        if let Some(description) = &repo.description {
            p { class: "mt-1 text-sm text-muted-foreground", "{description}" }
        }
        if !repo.topics.is_empty() {
            ul { class: "mt-2 flex flex-wrap gap-1.5", aria_label: "Topics",
                for topic in &repo.topics { li { {topic_chip(topic)} } }
            }
        }
        div { class: "mt-2 flex flex-wrap items-center gap-4 text-xs text-muted-foreground",
            if let Some(language) = &repo.primary_language {
                span { class: "repo-language", span { class: "language-dot", style: "background: {language.color}", aria_hidden: "true" } "{language.name}" }
            }
            span { {count_label(repo.stars.max(0) as usize, "star", "stars")} }
            span { "{repo.forks} forks" }
            span { "{repo.default_branch}" }
            time { datetime: repo.updated_at.clone(), "Updated {compact_date(&repo.updated_at)}" }
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
    // Trending follows the filter too, and its heading says what it is narrowed to.
    let trending_scope = if model.find.is_empty() {
        "Most starred of the 100 newest public repositories.".to_owned()
    } else {
        format!(
            "Most starred of the 100 newest public repositories matching “{}”.",
            model.find
        )
    };
    // With nothing trending the column is left out, and the work lists take the whole width.
    let solo = model.trending.is_empty();
    rsx! {
        div { class: if solo { "page-shell discovery-dashboard discovery-dashboard-solo" } else { "page-shell discovery-dashboard" },
            section { class: "discovery-dashboard-main", aria_labelledby: "dashboard-repositories-title",
                div { class: "discovery-summary",
                    NavLink { href: "/issues?tab=assigned&state=open", class: "focus-ring discovery-summary-card",
                        span { class: "discovery-summary-count", "{model.assigned_issues}" }
                        span { class: "discovery-summary-label", "Assigned issues" }
                    }
                    NavLink { href: "/pulls?tab=involved&state=open", class: "focus-ring discovery-summary-card",
                        span { class: "discovery-summary-count", "{model.open_pull_requests}" }
                        span { class: "discovery-summary-label", "Open pull requests" }
                    }
                }
                header { class: "discovery-dashboard-header",
                    h1 { id: "dashboard-repositories-title",
                        "Your repositories"
                        span { class: "discovery-repository-count", aria_label: count_label(model.repositories.len(), "repository", "repositories"), "{model.repositories.len()}" }
                    }
                    NavLink { href: "/new", class: "focus-ring discovery-new-repository", "New repository" }
                }
                form { class: "discovery-find", method: "get", action: "/", role: "search",
                    label { r#for: "dashboard-find", class: "sr-only", "Find a repository" }
                    input { id: "dashboard-find", r#type: "search", name: "q", value: model.find.clone(), maxlength: 100, class: "filter-search", placeholder: "Find a repository…" }
                }
                if model.repositories.is_empty() && !model.find.is_empty() {
                    div { class: "discovery-dashboard-empty",
                        h2 { "No repositories match “{model.find}”" }
                        p { NavLink { href: "/", class: "focus-ring discovery-link", "Clear the filter" } }
                    }
                } else if model.repositories.is_empty() {
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
                                    span { {count_label(repo.stars.max(0) as usize, "star", "stars")} }
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
                                        span { {count_label(shared.repository.stars.max(0) as usize, "star", "stars")} }
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
            if !solo {
                aside { class: "discovery-dashboard-trending", aria_labelledby: "dashboard-trending-title",
                    header { class: "discovery-trending-header",
                        h2 { id: "dashboard-trending-title", "Trending" }
                        NavLink { href: "/explore?sort=stars", class: "focus-ring discovery-link", "Explore" }
                    }
                    p { class: "discovery-trending-scope", "{trending_scope}" }
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
                                    span { {count_label(repo.stars.max(0) as usize, "star", "stars")} }
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

/// A link to one page of Explore, keeping the filters and the search. Page 1 has no page number.
fn explore_url(model: &ExploreModel, query: &str, topic: &str, page: usize) -> String {
    let mut parts = vec![format!(
        "sort={}",
        crate::webui::encode_component(&model.sort)
    )];
    if !query.is_empty() {
        parts.push(format!("q={}", crate::webui::encode_component(query)));
    }
    if !model.language.is_empty() {
        parts.push(format!(
            "language={}",
            crate::webui::encode_component(&model.language)
        ));
    }
    if !topic.is_empty() {
        parts.push(format!("topic={}", crate::webui::encode_component(topic)));
    }
    if !model.kind.is_empty() && model.kind != "all" {
        parts.push(format!(
            "type={}",
            crate::webui::encode_component(&model.kind)
        ));
    }
    if page > 1 {
        parts.push(format!("page={page}"));
    }
    format!("/explore?{}", parts.join("&"))
}

/// "All languages", then every language found, keeping a chosen one that no repository has.
fn language_options(model: &ExploreModel) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new("", "All languages")];
    let chosen = (!model.language.is_empty() && !model.languages.contains(&model.language))
        .then_some(&model.language);
    for language in chosen.into_iter().chain(&model.languages) {
        options.push(SelectOption::new(language.as_str(), language.as_str()));
    }
    options
}

pub fn explore(model: ExploreModel) -> Element {
    let clear_href = explore_url(&model, "", &model.topic, 1);
    let topic_clear_href = explore_url(&model, &model.query, "", 1);
    let count = count_label(model.repositories.len(), "repository", "repositories");
    let newer_href =
        (model.page > 1).then(|| explore_url(&model, &model.query, &model.topic, model.page - 1));
    let older_href = model
        .has_older
        .then(|| explore_url(&model, &model.query, &model.topic, model.page + 1));
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
            form { id: "explore-filters", method: "get", action: "/explore", class: "filter-toolbar explore-filters panel mb-4 p-4",
                if !model.topic.is_empty() { input { r#type: "hidden", name: "topic", value: model.topic.clone() } }
                label { r#for: "explore-q", class: "sr-only", "Search repositories" }
                input { id: "explore-q", r#type: "search", name: "q", value: model.query.clone(), maxlength: 256, class: "filter-search", placeholder: "Search repositories",
                    onchange: move |_| submit_form("explore-filters") }
                label { class: "filter-control",
                    span { "Sort:" }
                    DraftSelect { name: "sort", initial_value: model.sort.to_string(), on_value: move |_: String| submit_form("explore-filters"), options: vec![
                        SelectOption::new("recent", "Newest"),
                        SelectOption::new("updated", "Recently updated"),
                        SelectOption::new("stars", "Most stars"),
                        SelectOption::new("forks", "Most forks"),
                    ] }
                }
                label { class: "filter-control",
                    span { "Type:" }
                    DraftSelect { name: "type", initial_value: if model.kind.is_empty() { "all".to_owned() } else { model.kind.clone() }, on_value: move |_: String| submit_form("explore-filters"), options: vec![
                        SelectOption::new("all", "All"),
                        SelectOption::new("sources", "Sources"),
                        SelectOption::new("forks", "Forks"),
                        SelectOption::new("archived", "Archived"),
                    ] }
                }
                label { class: "filter-control",
                    span { "Language:" }
                    DraftSelect { name: "language", initial_value: model.language.clone(), on_value: move |_: String| submit_form("explore-filters"), options: language_options(&model) }
                }
                button { r#type: "submit", class: "btn explore-apply", "Apply" }
            }
            if !model.topic.is_empty() {
                div { class: "mb-3 flex flex-wrap items-center gap-2 text-sm", aria_label: "Topic filter",
                    span { class: "text-muted-foreground", "Topic" }
                    span { class: "inline-flex items-center gap-1 rounded-full border border-border py-0.5 pl-2.5 pr-1 text-xs font-medium",
                        "{model.topic}"
                        NavLink { href: topic_clear_href, class: "icon-tooltip focus-ring rounded-full px-1 text-muted-foreground hover:text-foreground", aria_label: Some(format!("Remove topic {}", model.topic)), "×" }
                    }
                }
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
                {page_links(newer_href, older_href, "Explore pages")}
            }
        }
    }
}

/// The result types a search offers: the tab name in the address, its label and its icon.
const SEARCH_TABS: [(&str, &str); 6] = [
    ("repos", "Repositories"),
    ("code", "Code"),
    ("commits", "Commits"),
    ("issues", "Issues"),
    ("pulls", "Pull requests"),
    ("users", "People"),
];

/// The sort choices of a result type. Types that are not listed here have a single order.
fn search_sorts(tab: &str) -> Vec<SelectOption> {
    match tab {
        "repos" => vec![
            SelectOption::new("", "Best match"),
            SelectOption::new("stars", "Most stars"),
            SelectOption::new("forks", "Most forks"),
            SelectOption::new("updated", "Recently updated"),
        ],
        "issues" | "pulls" => vec![
            SelectOption::new("", "Best match"),
            SelectOption::new("newest", "Newest"),
            SelectOption::new("oldest", "Oldest"),
        ],
        _ => Vec::new(),
    }
}

/// A result count as a badge. A count at the cap is shown as more than the cap.
fn count_badge(count: usize) -> String {
    if count >= SEARCH_COUNT_CAP {
        format!("{SEARCH_COUNT_CAP}+")
    } else {
        count.to_string()
    }
}

/// The number shown beside a result type. Code and commits show the page they list, marked as
/// more when an older page has results, and only while their tab is open.
fn search_tab_count(model: &SearchModel, tab: &str) -> Option<String> {
    match tab {
        "repos" => Some(count_badge(model.counts.repositories)),
        "issues" => Some(count_badge(model.counts.issues)),
        "pulls" => Some(count_badge(model.counts.pulls)),
        "users" => Some(count_badge(model.counts.users)),
        "code" | "commits" if model.tab == tab => {
            let shown = if tab == "code" {
                model.code.len()
            } else {
                model.commits.len()
            };
            Some(if model.has_older {
                format!("{shown}+")
            } else {
                shown.to_string()
            })
        }
        _ => None,
    }
}

/// The notes above code and commit results: where the budget stopped the walk, the repositories
/// the walk reached but could not read, and the repositories with more matches than are shown.
fn search_notices(model: &SearchModel) -> Element {
    rsx! {
        if let Some(scan) = &model.scan {
            p { role: "status", class: "mb-3 text-sm text-muted-foreground", "Searched {scan.searched} of {scan.total} repositories, narrow with repo:/user:/org:" }
        }
        if model.unsearched > 0 {
            p { role: "status", class: "mb-3 text-sm text-muted-foreground", {count_label(model.unsearched, "repository", "repositories")}, " could not be searched." }
        }
        for name in &model.truncated {
            p { role: "status", class: "mb-3 text-sm text-muted-foreground", "More matches in {name}, narrow with repo:{name} to see them." }
        }
    }
}

/// The issue or pull request results: state icon and state, title, number, author and an excerpt.
fn issue_results(model: &SearchModel, kind: Kind, issues: &[IssueCard], none: &str) -> Element {
    // The rows are the issue list's rows, so they take its stylesheet.
    rsx! {
        style { dangerous_inner_html: include_str!("../repos/style.css") }
        div { class: "panel overflow-hidden repo-content",
            if issues.is_empty() { div { class: "px-4 py-14 text-center", h2 { class: "font-semibold", "{none}" } } }
            for issue in issues {
                article { class: "collaboration-row",
                    div { class: "collaboration-row-icon", title: issue.state.clone(), {item_icon(kind, &issue.state)} }
                    div { class: "collaboration-row-content",
                        div { class: "collaboration-row-title",
                            NavLink { href: format!("{}/{}/{}", repository_url(&issue.owner, &issue.repo), kind.family(), issue.number), class: "collaboration-title-link", "{issue.title}" }
                        }
                        p { class: "collaboration-row-meta",
                            span { class: state_class(&issue.state), "{issue.state}" }
                            " {issue.owner}/{issue.repo} #{issue.number}"
                            if !issue.author.is_empty() { " opened by {issue.author}" }
                            " · Updated {compact_date(&issue.updated_at)}"
                        }
                        if !issue.excerpt.is_empty() { p { class: "collaboration-row-excerpt", "{issue.excerpt}" } }
                    }
                }
            }
            {search_pager(model)}
        }
    }
}

/// The parts of `line` split at the matches of `needle`, ignoring ASCII case. Each part says
/// whether it is a match.
fn highlight_parts(line: &str, needle: &str) -> Vec<(String, bool)> {
    let mut parts = Vec::new();
    if needle.is_empty() {
        return vec![(line.to_owned(), false)];
    }
    let mut plain = 0;
    let mut at = 0;
    while at + needle.len() <= line.len() {
        // A match starts on a character boundary, and it ends on one too because its bytes equal
        // the needle's, which is valid UTF-8.
        let matched = line.is_char_boundary(at)
            && line.as_bytes()[at..at + needle.len()].eq_ignore_ascii_case(needle.as_bytes());
        if matched {
            if plain < at {
                parts.push((line[plain..at].to_owned(), false));
            }
            parts.push((line[at..at + needle.len()].to_owned(), true));
            at += needle.len();
            plain = at;
        } else {
            at += 1;
        }
    }
    if plain < line.len() {
        parts.push((line[plain..].to_owned(), false));
    }
    parts
}

/// A line of code with each match of the search text in a `mark`. The renderer escapes the text.
fn highlighted_text(line: &str, needle: &str) -> Element {
    let parts = highlight_parts(line, needle);
    rsx! {
        for (text, matched) in parts {
            if matched { mark { class: "search-match", "{text}" } } else { "{text}" }
        }
    }
}

fn search_results(model: &SearchModel) -> Element {
    match model.tab.as_str() {
        "issues" => issue_results(model, Kind::Issue, &model.issues, "No issues found"),
        "pulls" => issue_results(model, Kind::Pull, &model.pulls, "No pull requests found"),
        "commits" => rsx! {
            {search_notices(model)}
            div { class: "panel overflow-hidden",
                if model.commits.is_empty() { div { class: "px-4 py-14 text-center",
                    h2 { class: "font-semibold", "No commits found" }
                    p { class: "mt-2 text-sm text-muted-foreground", "Commit messages in the repositories you can read are searched. Add repo:owner/name to search one." }
                } }
                for hit in &model.commits {
                    article { class: "border-b border-border px-4 py-4",
                        NavLink { href: format!("{}/commit/{}", repository_url(&hit.owner, &hit.repo), hit.sha), class: "text-base font-semibold text-primary hover:underline", "{hit.summary}" }
                        p { class: "mt-2 text-xs text-muted-foreground", "{hit.owner}/{hit.repo} · {hit.author} committed {compact_date(&hit.date)} · ", code { "{hit.sha.get(..7).unwrap_or(&hit.sha)}" } }
                    }
                }
                {search_pager(model)}
            }
        },
        "code" => rsx! {
            {search_notices(model)}
            div { class: "panel overflow-hidden",
                if model.code.is_empty() { div { class: "px-4 py-14 text-center",
                    h2 { class: "font-semibold", "No code found" }
                    p { class: "mt-2 text-sm text-muted-foreground", "Files on the default branch of the repositories you can read are searched. Add repo:owner/name to search one." }
                } }
                for hit in &model.code {
                    article { class: "border-b border-border px-4 py-3",
                        p { class: "text-sm",
                            NavLink { href: format!("{}/blob/{}/{}#L{}", repository_url(&hit.owner, &hit.repo), crate::webui::encode_component(&hit.branch), crate::webui::pages::repos::ref_path::encode_path(&hit.path), hit.line_number), class: "font-semibold text-primary hover:underline", "{hit.owner}/{hit.repo}: {hit.path}" }
                        }
                        pre { class: "mt-2 overflow-x-auto rounded-md bg-muted px-3 py-2 text-xs", code { "{hit.line_number}  ", {highlighted_text(&hit.line, &model.highlight)} } }
                    }
                }
                {search_pager(model)}
            }
        },
        "users" => rsx! {
            div { class: "panel overflow-hidden",
                if model.users.is_empty() { div { class: "px-4 py-14 text-center", h2 { class: "font-semibold", "No people found" } } }
                for user in &model.users {
                    article { class: "flex items-start gap-3 border-b border-border px-4 py-4",
                        Avatar { name: user.username.clone(), size: 40 }
                        div { class: "min-w-0",
                            h2 { class: "text-sm font-semibold", NavLink { href: format!("/{}", crate::webui::encode_component(&user.username)), class: "text-primary hover:underline", "{user.display_name.as_deref().unwrap_or(&user.username)}" } }
                            p { class: "text-xs text-muted-foreground", "@{user.username}" }
                            if let Some(bio) = &user.bio { p { class: "mt-1 text-sm text-muted-foreground", "{bio}" } }
                        }
                    }
                }
                {search_pager(model)}
            }
        },
        _ => rsx! {
            div { class: "panel overflow-hidden",
                if model.repositories.is_empty() { div { class: "px-4 py-14 text-center",
                    h2 { class: "font-semibold", "No repositories found for \"{model.query}\"" }
                    p { class: "mt-2 text-sm text-muted-foreground", "Try a broader search term." }
                } }
                for repo in &model.repositories { {repository_card(repo)} }
                {search_pager(model)}
            }
        },
    }
}

pub fn search(model: SearchModel) -> Element {
    let sorts = search_sorts(&model.tab);
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-5",
                h1 { class: "text-2xl font-semibold tracking-tight", if model.query.trim().is_empty() { "Search Codeyard" } else { "Search results" } }
                if !model.query.trim().is_empty() { p { class: "mt-1 text-sm text-muted-foreground", "Results for \"{model.query}\"" } }
            }
            form { id: "search-form", action: "/search", method: "get", class: "mb-6 flex flex-wrap items-end gap-3",
                div { class: "flex-1",
                    label { r#for: "search-q", class: "sr-only", "Search repositories, code, commits, issues, pull requests and people" }
                    input { id: "search-q", name: "q", "value": model.query.clone(), maxlength: 256, class: INPUT, placeholder: "Search repositories, code, commits, issues, pull requests and people" }
                }
                input { r#type: "hidden", name: "type", value: model.tab.clone() }
                if !sorts.is_empty() {
                    label { class: "filter-control",
                        span { "Sort:" }
                        DraftSelect { name: "sort", initial_value: model.sort.clone(), on_value: move |_: String| submit_form("search-form"), options: sorts }
                    }
                }
                button { r#type: "submit", class: BUTTON, "Search" }
            }
            div { class: "grid grid-cols-1 gap-6 md:grid-cols-[220px_1fr]",
                nav { class: "flex flex-col gap-2", aria_label: "Search result type",
                    for (tab, label) in SEARCH_TABS {
                        NavLink { href: search_tab_url(&model.query, tab), class: if model.tab == tab { "focus-ring flex items-center justify-between rounded-md bg-accent px-3 py-2 text-sm font-medium" } else { "focus-ring flex items-center justify-between rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-accent" },
                            span { "{label}" }
                            if let Some(count) = search_tab_count(&model, tab) { span { class: "rounded-full bg-muted px-2 py-0.5 text-xs", "{count}" } }
                        }
                    }
                }
                section { class: "min-w-0", aria_label: "Search results",
                    if model.query.trim().is_empty() {
                        div { class: "panel px-4 py-14 text-center",
                            h2 { class: "text-base font-semibold", "Find something on Codeyard" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Search public repositories, code, commits, issues, pull requests and people." }
                        }
                    } else {
                        {search_results(&model)}
                    }
                }
            }
        }
    }
}

/// The URL of one page of the Stars list, with its order and text.
fn stars_url(sort: &str, query: &str, page: usize) -> String {
    let mut parts = vec![format!("sort={}", crate::webui::encode_component(sort))];
    if !query.is_empty() {
        parts.push(format!("q={}", crate::webui::encode_component(query)));
    }
    if page > 1 {
        parts.push(format!("page={page}"));
    }
    format!("/stars?{}", parts.join("&"))
}

/// The filter bar of a Stars list: a text box and a sort menu, submitted to `action`. `extra`
/// holds hidden fields the list needs, such as the profile's tab.
pub fn stars_toolbar(
    form_id: &'static str,
    action: &str,
    query: &str,
    sort: &str,
    extra: Element,
) -> Element {
    let input_id = format!("{form_id}-q");
    rsx! {
        form { id: form_id, method: "get", action: action.to_owned(), class: "filter-toolbar explore-filters panel mb-4 p-4",
            {extra}
            label { r#for: input_id.clone(), class: "sr-only", "Filter starred repositories" }
            input { id: input_id, r#type: "search", name: "q", value: query.to_owned(), maxlength: 256, class: "filter-search", placeholder: "Filter starred repositories",
                onchange: move |_| submit_form(form_id) }
            label { class: "filter-control",
                span { "Sort:" }
                DraftSelect { name: "sort", initial_value: sort.to_owned(), on_value: move |_: String| submit_form(form_id), options: vec![
                    SelectOption::new("recent", "Recently starred"),
                    SelectOption::new("stars", "Most stars"),
                    SelectOption::new("name", "Name"),
                ] }
            }
            button { r#type: "submit", class: "btn explore-apply", "Apply" }
        }
    }
}

/// A starred repository with its Unstar button. The button posts to the repository's star form,
/// which sends the viewer back to `return_to`, this list.
fn starred_repository(repo: &RepositoryCard, csrf_token: &str, return_to: &str) -> Element {
    let action = format!("{}/_forms/unstar", repository_url(&repo.owner, &repo.name));
    rsx! {
        article { class: "flex flex-wrap items-start justify-between gap-3 border-b border-border px-4 py-4 sm:px-5",
            div { class: "min-w-0 flex-1", {repository_summary(repo)} }
            form { method: "post", action, class: "stars-unstar-form",
                input { r#type: "hidden", name: "csrf_token", value: csrf_token.to_owned() }
                input { r#type: "hidden", name: "return_to", value: return_to.to_owned() }
                button { r#type: "submit", class: "btn btn-sm stars-unstar", "aria-label": "Unstar {repo.owner}/{repo.name}", "Unstar" }
            }
        }
    }
}

/// The viewer's starred repositories, narrowed by sort and text, with a filter box above them.
pub fn stars(model: StarsModel) -> Element {
    let clear_href = stars_url(&model.sort, "", 1);
    let newer_href = (model.page > 1).then(|| stars_url(&model.sort, &model.query, model.page - 1));
    let older_href = model
        .has_older
        .then(|| stars_url(&model.sort, &model.query, model.page + 1));
    let here = stars_url(&model.sort, &model.query, model.page);
    let heading = if model.query.is_empty() {
        "Starred repositories".to_owned()
    } else {
        format!("Starred repositories matching “{}”", model.query)
    };
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-6 space-y-1",
                h1 { class: "text-2xl font-semibold tracking-tight", "Your stars" }
                p { class: "text-sm text-muted-foreground", "Repositories you starred that you can still read." }
            }
            {stars_toolbar("stars-filters", "/stars", &model.query, &model.sort, rsx! {})}
            h2 { class: "mb-3 text-sm font-semibold", "{heading}" }
            section { class: "panel overflow-hidden", aria_label: "{heading}",
                if model.repositories.is_empty() {
                    div { class: "px-4 py-14 text-center",
                        if !model.query.is_empty() {
                            h3 { class: "text-base font-semibold", "No starred repositories match “{model.query}”" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Try a different name or description." }
                            NavLink { href: clear_href, class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Clear filter" }
                        } else if model.page > 1 {
                            h3 { class: "text-base font-semibold", "No starred repositories on this page" }
                            NavLink { href: stars_url(&model.sort, "", 1), class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Show the first page" }
                        } else {
                            h3 { class: "text-base font-semibold", "No starred repositories yet" }
                            p { class: "mt-2 text-sm text-muted-foreground", "Star a repository you can read to find it here again." }
                            NavLink { href: "/explore", class: "focus-ring mt-3 inline-block text-sm text-primary hover:underline", "Explore repositories" }
                        }
                    }
                }
                for repo in &model.repositories { {starred_repository(repo, &model.csrf_token, &here)} }
                {page_links(newer_href, older_href, "Starred repository pages")}
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

/// The options of a template select: an empty choice for none, then each listed name.
fn template_options(none: &str, names: &[&str]) -> Vec<SelectOption> {
    std::iter::once(SelectOption::new("", none))
        .chain(names.iter().map(|name| SelectOption::new(*name, *name)))
        .collect()
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
                            div { p { class: "flex items-center gap-2 font-medium", {profile_icon(ProfileIcon::Globe)} "Public" } p { class: "text-sm text-muted-foreground", "Anyone who can reach this server can see it. You choose who can commit." } }
                        }
                        label { class: if is_private() { "flex cursor-pointer items-start gap-3 rounded-md border border-success-emphasis p-3 ring-1 ring-success-emphasis" } else { "flex cursor-pointer items-start gap-3 rounded-md border border-border p-3" },
                            input { r#type: "radio", name: "visibility", value: "private", checked: is_private(), onchange: move |_| is_private.set(true), class: "mt-1" }
                            div { p { class: "flex items-center gap-2 font-medium", {profile_icon(ProfileIcon::Lock)} "Private" } p { class: "text-sm text-muted-foreground", "You choose who can see and commit to this repository." } }
                        }
                    }
                    fieldset { class: "collaboration-option-group",
                        legend { class: "text-sm font-medium", "Initialize this repository with" }
                        label { class: "repo-check-label",
                            DraftCheckbox { name: "readme", initial_checked: form.readme, id: Some("repo-readme".to_owned()) }
                            span { "Add a README file" }
                        }
                        p { class: "text-xs text-muted-foreground", "Creates README.md with the repository name and description." }
                        div {
                            label { r#for: "repo-gitignore", class: "mb-2 block text-sm font-medium", ".gitignore template" }
                            DraftSelect { id: Some("repo-gitignore".to_owned()), name: "gitignore", class: INPUT, initial_value: form.gitignore.clone(), options: template_options("No .gitignore", &GITIGNORE_CHOICES) }
                        }
                        div {
                            label { r#for: "repo-license", class: "mb-2 block text-sm font-medium", "License" }
                            DraftSelect { id: Some("repo-license".to_owned()), name: "license", class: INPUT, initial_value: form.license.clone(), options: template_options("No license", &LICENSE_CHOICES) }
                            p { class: "mt-1 text-xs text-muted-foreground", "Adds a LICENSE file with the license's standard text." }
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

    /// The markup from `marker` through the first `close` after it.
    fn element_at<'a>(html: &'a str, marker: &str, close: &str) -> &'a str {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker}"));
        let end = html[at..]
            .find(close)
            .map_or(html.len(), |end| at + end + close.len());
        &html[at..end]
    }

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
                query: "python".into(),
                language: String::new(),
                topic: String::new(),
                kind: String::new(),
                languages: vec![],
                page: 3,
                has_older: false,
                repositories: vec![],
            }))
        });
        assert!(html.contains("No repositories match"));
        assert!(html.contains("python"));
        assert!(html.contains("Clear search"));
        assert!(html.contains("sort=stars"));
        assert!(!html.contains("page=3"));
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "recent".into(),
                query: String::new(),
                language: String::new(),
                topic: String::new(),
                kind: String::new(),
                languages: vec![],
                page: 1,
                has_older: false,
                repositories: vec![],
            }))
        });
        assert!(!html.contains("Clear search"));
    }

    #[test]
    fn search_renders_escaped_content_real_links_and_total_counts() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "<script>alert(1)</script>&".into(),
                tab: "issues".into(),
                issues: vec![IssueCard {
                    owner: "alice".into(),
                    repo: "demo".into(),
                    number: 4,
                    title: "<img src=x onerror=alert(1)>".into(),
                    ..Default::default()
                }],
                counts: SearchCounts {
                    issues: 1,
                    ..Default::default()
                },
                ..Default::default()
            }))
        });
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
    fn count_badges_show_the_cap_as_more_than_the_cap() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "needle".into(),
                tab: "repos".into(),
                counts: SearchCounts {
                    repositories: SEARCH_COUNT_CAP,
                    issues: SEARCH_COUNT_CAP - 1,
                    ..Default::default()
                },
                ..Default::default()
            }))
        });
        assert!(html.contains(">1000+</span>"), "{html}");
        assert!(html.contains(">999</span>"), "{html}");
    }

    #[test]
    fn code_hit_links_encode_each_path_segment() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "needle".into(),
                tab: "code".into(),
                code: vec![CodeHit {
                    owner: "alice".into(),
                    repo: "demo".into(),
                    branch: "main".into(),
                    path: "src/a#b?c%d.rs".into(),
                    line_number: 3,
                    line: "needle".into(),
                }],
                ..Default::default()
            }))
        });
        assert!(
            html.contains("/alice/demo/blob/main/src/a%23b%3Fc%25d.rs#L3"),
            "{html}"
        );
    }

    #[test]
    fn issue_and_pull_cards_show_state_icon_number_author_and_excerpt() {
        let issue = IssueCard {
            owner: "alice".into(),
            repo: "demo".into(),
            number: 1,
            title: "Crash when saving an empty file".into(),
            state: "open".into(),
            author: "bob".into(),
            excerpt: "I reproduced the crash".into(),
            created_at: "2026-10-18T09:00:00".into(),
            updated_at: "2026-10-18".into(),
        };
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "crash".into(),
                tab: "issues".into(),
                issues: vec![issue.clone()],
                counts: SearchCounts {
                    issues: 1,
                    ..Default::default()
                },
                ..Default::default()
            }))
        });
        assert!(html.contains("Crash when saving an empty file"), "{html}");
        assert!(html.contains("alice/demo #1 opened by bob"), "{html}");
        assert!(html.contains("I reproduced the crash"), "{html}");
        assert!(html.contains("collaboration-icon-open"), "{html}");
        assert!(html.contains("collaboration-state-open"), "{html}");
        assert!(html.contains("/alice/demo/issues/1"), "{html}");
        // A merged pull request has its own icon and state colour.
        let pull = IssueCard {
            state: "merged".into(),
            author: String::new(),
            excerpt: String::new(),
            ..issue
        };
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "crash".into(),
                tab: "pulls".into(),
                pulls: vec![pull.clone()],
                counts: SearchCounts {
                    pulls: 1,
                    ..Default::default()
                },
                ..Default::default()
            }))
        });
        assert!(html.contains("collaboration-icon-merged"), "{html}");
        assert!(html.contains("collaboration-state-merged"), "{html}");
        assert!(html.contains("/alice/demo/pulls/1"), "{html}");
        assert!(!html.contains("opened by"), "{html}");
    }

    #[test]
    fn code_matches_are_marked_and_the_line_stays_escaped() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "old".into(),
                tab: "code".into(),
                highlight: "old".into(),
                code: vec![CodeHit {
                    owner: "alice".into(),
                    repo: "demo".into(),
                    branch: "main".into(),
                    path: "old.rs".into(),
                    line_number: 1,
                    line: "fn old() { <b>OLD</b> }".into(),
                }],
                ..Default::default()
            }))
        });
        assert!(
            html.contains("<mark class=\"search-match\">old</mark>"),
            "{html}"
        );
        assert!(
            html.contains("<mark class=\"search-match\">OLD</mark>"),
            "{html}"
        );
        assert!(html.contains("&#60;b&#62;"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
    }

    #[test]
    fn highlight_parts_split_at_each_match_ignoring_ascii_case() {
        let parts =
            |line: &str, needle: &str| -> Vec<(String, bool)> { highlight_parts(line, needle) };
        let owned = |items: &[(&str, bool)]| -> Vec<(String, bool)> {
            items
                .iter()
                .map(|(text, matched)| ((*text).to_owned(), *matched))
                .collect()
        };
        assert_eq!(
            parts("fn Old() old", "old"),
            owned(&[("fn ", false), ("Old", true), ("() ", false), ("old", true)])
        );
        // Non-ASCII text stays whole, and the match after it is still found.
        assert_eq!(
            parts("é old", "OLD"),
            owned(&[("é ", false), ("old", true)])
        );
        assert_eq!(
            parts("nothing here", "zzz"),
            owned(&[("nothing here", false)])
        );
        assert_eq!(parts("unchanged", ""), owned(&[("unchanged", false)]));
    }

    #[test]
    fn people_results_use_the_avatar_component() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "alice".into(),
                tab: "users".into(),
                users: vec![UserCard {
                    username: "alice".into(),
                    display_name: Some("Alice".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }))
        });
        assert!(html.contains("src=\"/avatars/alice\""), "{html}");
        assert!(html.contains("class=\"avatar\""), "{html}");
        assert!(!html.contains(">@</div>"), "{html}");
    }

    #[test]
    fn stars_rows_carry_one_unstar_form_that_returns_to_the_list() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(stars(StarsModel {
                sort: "stars".into(),
                query: String::new(),
                page: 2,
                has_older: false,
                repositories: vec![RepositoryCard {
                    owner: "bob".into(),
                    name: "tool".into(),
                    stars: 3,
                    ..Default::default()
                }],
                csrf_token: "stars-token".into(),
            }))
        });
        assert_eq!(
            html.matches("action=\"/bob/tool/_forms/unstar\"").count(),
            1,
            "{html}"
        );
        let action = html.find("action=\"/bob/tool/_forms/unstar\"").unwrap();
        let open = html[..action].rfind("<form").unwrap();
        let form = element_at(&html[open..], "<form", "</form>");
        assert!(form.contains("method=\"post\""), "{form}");
        assert!(form.contains("name=\"csrf_token\""), "{form}");
        assert!(form.contains("value=\"stars-token\""), "{form}");
        assert!(form.contains("name=\"return_to\""), "{form}");
        assert!(form.contains("/stars?sort=stars"), "{form}");
        assert!(form.contains("aria-label=\"Unstar bob/tool\""), "{form}");
        // The filter bar above the list is the one /stars shares with the profile tab.
        assert!(html.contains("id=\"stars-filters\""), "{html}");
        assert!(html.contains("action=\"/stars\""), "{html}");
    }

    #[test]
    fn empty_repository_search_names_the_query_and_suggests_a_broader_term() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(search(SearchModel {
                query: "zzz".into(),
                tab: "repos".into(),
                ..Default::default()
            }))
        });
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
                ..Default::default()
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
                ..Default::default()
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
    fn creation_form_offers_initial_files_and_keeps_their_drafts() {
        let html = dioxus_ssr::render_element(new_repository(
            "alice".into(),
            "csrf-test".into(),
            RepositoryCreationChoices::default(),
            NewRepositoryForm {
                readme: true,
                gitignore: "Rust".into(),
                license: "MIT".into(),
                ..Default::default()
            },
            None,
        ));
        // The opening tag of the element that holds `marker`.
        let opening_tag = |marker: &str| {
            let at = html.find(marker).unwrap();
            let start = html[..at].rfind('<').unwrap();
            html[start..].split('>').next().unwrap().to_owned()
        };
        assert!(html.contains("Add a README file"));
        assert!(opening_tag("name=\"readme\"").contains("checked"));
        assert!(opening_tag("value=\"Rust\"").contains("selected"));
        assert!(opening_tag("value=\"MIT\"").contains("selected"));
        assert!(!opening_tag("value=\"Node\"").contains("selected"));
        assert!(html.contains("No .gitignore") && html.contains("No license"));
        for name in GITIGNORE_CHOICES.iter().chain(&LICENSE_CHOICES) {
            assert!(html.contains(&format!("value=\"{name}\"")), "{name}");
        }
    }

    #[test]
    fn visibility_choices_are_icon_cards_with_one_line_explanations() {
        let html = dioxus_ssr::render_element(new_repository(
            "alice".into(),
            "csrf-test".into(),
            RepositoryCreationChoices::default(),
            NewRepositoryForm::default(),
            None,
        ));
        let visibility = html
            .split("collaboration-option-group")
            .nth(1)
            .unwrap()
            .split("</fieldset>")
            .next()
            .unwrap();
        assert_eq!(visibility.matches("<svg").count(), 2);
        assert!(visibility
            .contains("Anyone who can reach this server can see it. You choose who can commit."));
        assert!(visibility.contains("You choose who can see and commit to this repository."));
    }

    #[test]
    fn explore_keeps_its_filters_and_pages_through_the_results() {
        let page = |page: usize, has_older: bool| {
            crate::webui::tests::in_scope(|| {
                dioxus_ssr::render_element(explore(ExploreModel {
                    sort: "stars".into(),
                    query: "Rust & Git".into(),
                    language: String::new(),
                    topic: String::new(),
                    kind: String::new(),
                    languages: vec![],
                    page,
                    has_older,
                    repositories: vec![RepositoryCard {
                        owner: "alice".into(),
                        name: "demo".into(),
                        description: Some("Rust & Git".into()),
                        ..Default::default()
                    }],
                }))
            })
        };
        let second = page(2, true);
        assert!(second.contains("method=\"get\""));
        assert!(second.contains("action=\"/explore\""));
        assert!(second.contains("Rust &#38; Git"));
        assert!(second.contains("1 repository matching “Rust &#38; Git”"));
        assert!(!second.contains("Public repositories"));
        assert!(second.contains("/alice/demo"));
        // The page size is fixed, so no size control is offered. Newer leads to page 1, which has
        // no page number, and Older leads to page 3.
        assert!(!second.contains("name=\"limit\""));
        assert!(!second.contains("Show more repositories"));
        assert!(second.contains("Newer"));
        assert!(second.contains("Older"));
        assert!(second.contains("page=3"));
        assert!(!second.contains("page=1"));
        let first = page(1, false);
        assert!(!first.contains("Newer"));
        assert!(!first.contains("Older"));
    }

    #[test]
    fn explore_topic_filter_is_a_removable_chip_and_cards_link_their_topics() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "stars".into(),
                query: "Rust".into(),
                language: String::new(),
                topic: "git".into(),
                kind: String::new(),
                languages: vec![],
                page: 1,
                has_older: false,
                repositories: vec![RepositoryCard {
                    owner: "alice".into(),
                    name: "demo".into(),
                    topics: vec!["forge".into()],
                    ..Default::default()
                }],
            }))
        });
        // The search form carries the topic, and the chip's remove link drops only the topic.
        assert!(html.contains("name=\"topic\" value=\"git\""));
        assert!(html.contains("aria-label=\"Topic filter\""));
        assert!(html.contains("aria-label=\"Remove topic git\""));
        assert!(html.contains("q=Rust"));
        assert!(!html.contains("topic=git"));
        // Each card links its topics to their Explore list.
        assert!(html.contains("href=\"/explore?topic=forge\""));
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
                topics: Vec::new(),
                is_private: number == 1,
                stars: number,
                forks: 2,
                default_branch: "main".into(),
                primary_language: None,
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
        let css = include_str!("../../input.css");
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
        assert!(html.contains("Your repositories"));
        assert!(html.contains("aria-label=\"0 repositories\""));
        assert!(html.contains("No repositories yet"));
        assert!(html.contains("href=\"/new\""));
        assert!(html.contains("Create a repository"));
        assert!(html.contains("href=\"/explore\""));
        assert!(!html.contains("class=\"discovery-repository-row\""));
        assert!(!html.contains("class=\"discovery-trending-list\""));
        assert!(!html.contains("Loading"));
    }

    #[test]
    fn dashboard_without_trending_drops_the_column_and_counts_link_to_their_lists() {
        let html = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            assigned_issues: 2,
            open_pull_requests: 1,
            ..Default::default()
        }));
        // Nothing is trending, so there is no Trending column, and the work takes the full width.
        assert!(html.contains("discovery-dashboard-solo"), "{html}");
        assert!(!html.contains("<aside"), "{html}");
        assert!(!html.contains("Trending"), "{html}");
        assert!(
            !html.contains("Nothing trending from other people yet."),
            "{html}"
        );
        // The two counts open the matching filtered lists.
        assert!(html.contains("href=\"/issues?tab=assigned"), "{html}");
        assert!(html.contains("href=\"/pulls?tab=involved"), "{html}");
        let with_trending = dioxus_ssr::render_element(home(HomeModel {
            username: Some("alice".into()),
            trending: vec![RepositoryCard {
                owner: "bob".into(),
                name: "popular".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        assert!(!with_trending.contains("discovery-dashboard-solo"));
        assert!(with_trending.contains("<aside"));
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
        assert!(html[shared..].contains("href=\"/bob/tool\""));
        assert!(html[shared..].contains("Shared tooling"));
        assert!(html[shared..].contains("Write"));
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
