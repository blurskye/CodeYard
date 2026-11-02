//! Package lists, installation instructions and native settings forms.
use crate::webui::{
    components::{DraftSelect, SelectOption},
    navigation::NavLink,
    pages::{
        fields::csrf,
        repos::chrome::{chrome, RepoChrome},
    },
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub name: String,
    pub profile: String,
    pub packages: String,
    pub organization: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub id: i64,
    pub name: String,
    pub owner: String,
    pub kind: String,
    pub visibility: String,
    pub href: String,
    pub versions: i64,
    pub updated: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub id: i64,
    pub name: String,
    pub digest: String,
    pub tags: Vec<String>,
    pub href: String,
    pub created: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub title: String,
    pub owner: Option<Owner>,
    pub repo: Option<Box<RepoChrome>>,
    pub explore: bool,
    pub base: String,
    pub kind: String,
    #[serde(default)]
    pub container_namespace: Option<String>,
    pub packages: Vec<Package>,
    pub newer: Option<String>,
    pub older: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub owner: Owner,
    pub package: Package,
    pub versions: Vec<Version>,
    pub selected: Option<Version>,
    pub newer: Option<String>,
    pub older: Option<String>,
    pub command: String,
    pub registry: Option<String>,
    pub linked_repository: String,
    pub repository_href: Option<String>,
    pub settings: bool,
    pub can_admin: bool,
    pub csrf_token: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    List(Box<Listing>),
    Package(Box<Detail>),
}

impl Page {
    pub fn repo_chrome(&self) -> Option<&RepoChrome> {
        match self {
            Self::List(list) => list.repo.as_deref(),
            Self::Package(_) => None,
        }
    }
}

fn owner_tabs(owner: &Owner) -> Element {
    let stars = format!("{}?tab=stars", owner.profile);
    rsx! {
        header { class: "mb-4", h1 { class: "text-2xl font-semibold", NavLink { href: owner.profile.clone(), "{owner.name}" } } }
        nav { class: "package-tabs mb-4", aria_label: "Profile sections",
            if owner.organization { NavLink { href: owner.profile.clone(), "Overview" } }
            NavLink { href: if owner.organization {format!("{}?tab=repositories",owner.profile)}else{owner.profile.clone()}, "Repositories" }
            NavLink { href: owner.packages.clone(), aria_current: "page", "Packages" }
            if !owner.organization { NavLink { href: stars, "Stars" } }
        }
    }
}

pub fn render(page: Page) -> Element {
    match page {
        Page::List(list) => {
            let content = rsx! {
                if let Some(owner) = &list.owner { {owner_tabs(owner)} }
                if list.explore {
                    nav { class: "package-tabs mb-4", aria_label: "Explore",
                        NavLink { href: "/explore", "Repositories" }
                        NavLink { href: "/explore/packages", aria_current: "page", "Packages" }
                    }
                }
                h1 { class: "text-2xl font-semibold mb-4", "{list.title}" }
                if let Some(namespace) = &list.container_namespace {
                    p { class: "text-sm text-muted mb-4", "Container registry namespace: ", code { "{namespace}" } }
                }
                form { method: "get", action: list.base.clone(), class: "filter-toolbar mb-4",
                    label { r#for: "package-type", "Type" }
                    DraftSelect { id: Some("package-type".to_owned()), name: "type", initial_value: list.kind.clone(), options: vec![
                        SelectOption::new("all", "All packages"),
                        SelectOption::new("container", "Containers"),
                        SelectOption::new("npm", "npm"),
                        SelectOption::new("maven", "Maven / Gradle"),
                        SelectOption::new("nuget", "NuGet"),
                        SelectOption::new("rubygems", "RubyGems"),
                    ] }
                    button { r#type: "submit", class: "btn", "Apply" }
                }
                div { class: "panel",
                    if list.packages.is_empty() { p { class: "p-4", "No packages found." } }
                    for package in list.packages {
                        article { class: "package-row p-4",
                            div {
                                h2 { class: "text-lg font-semibold", NavLink { href: package.href, "{package.owner} / {package.name}" } }
                                p { class: "text-sm text-muted", "{package.kind} · {package.versions} versions · Updated {package.updated}" }
                            }
                            span { class: "badge", "{package.visibility}" }
                        }
                    }
                }
                {crate::webui::pages::discovery::page_links(list.newer,list.older,"Package pages")}
            };
            match list.repo {
                Some(repo) => chrome(&repo, content),
                None => rsx! {div {class:"page-shell package-page",{content}}},
            }
        }
        Page::Package(detail) => rsx! { div { class: "page-shell package-page",
            {owner_tabs(&detail.owner)}
            header { class: "package-row mb-4",
                h1 { class: "text-2xl font-semibold", "{detail.package.name}" }
                span { class: "badge", "{detail.package.visibility}" }
            }
            nav { class: "package-tabs mb-4", aria_label: "Package sections",
                NavLink { href: detail.package.href.clone(), aria_current: if detail.settings {"false"}else{"page"}, "Versions" }
                if detail.can_admin { NavLink { href: format!("{}/settings",detail.package.href), aria_current: if detail.settings {"page"}else{"false"}, "Package settings" } }
            }
            if detail.settings {
                section { class: "panel p-4 mb-4",
                    h2 { class: "text-lg font-semibold mb-4", "Package settings" }
                    form { method: "post", action: format!("{}/settings",detail.package.href), class: "package-settings",
                        {csrf(&detail.csrf_token)}
                        input { r#type: "hidden", name: "package_id", value: detail.package.id.to_string() }
                        input { r#type: "hidden", name: "action", value: "settings" }
                        label { "Visibility"
                            DraftSelect { name: "visibility", initial_value: detail.package.visibility.clone(), options: vec![SelectOption::new("private","Private"),SelectOption::new("public","Public")] }
                        }
                        label { "Linked repository"
                            input { name: "repository", value: detail.linked_repository.clone(), placeholder: "Repository name (optional)", maxlength: "255" }
                        }
                        p { class: "text-sm text-muted", "The repository must belong to this owner. Private repository access also applies to linked packages." }
                        button { r#type: "submit", class: "btn btn-primary", "Save changes" }
                    }
                }
                section { class: "panel p-4 mb-4",
                    h2 { class: "text-lg font-semibold mb-4", "Delete this package" }
                    p { "Deleted packages and versions can be restored through the API for 30 days." }
                    form { method: "post", action: format!("{}/settings",detail.package.href), class: "package-settings",
                        {csrf(&detail.csrf_token)}
                        input { r#type: "hidden", name: "package_id", value: detail.package.id.to_string() }
                        input { r#type: "hidden", name: "action", value: "delete" }
                        label { "Type the package name to confirm" input { name: "confirm", required: true } }
                        button { r#type: "submit", class: "btn btn-danger", "Delete package" }
                    }
                }
            } else {
                section { class: "panel p-4 mb-4",
                    h2 { class: "text-lg font-semibold mb-4", if detail.package.kind=="container" {"Pull this container"}else{"Install this package"} }
                    if let Some(registry) = &detail.registry { p { if detail.package.kind=="npm" {"Add this line to .npmrc:"}else{"Registry endpoint:"} } pre { class: "package-command", code { "{registry}" } } }
                    pre { class: "package-command", code { "{detail.command}" } }
                    if let Some(href) = &detail.repository_href { p { "Linked repository: " NavLink { href: href.clone(), "{detail.owner.name}/{detail.linked_repository}" } } }
                }
            }
            if let Some(version) = &detail.selected {
                section { class: "panel p-4 mb-4", aria_label: "Version details",
                    h2 { class: "text-lg font-semibold", "{version.name}" }
                    p { class: "text-sm text-muted", "Published {version.created}" }
                    for tag in &version.tags {span {class:"badge","{tag}"}}
                    pre {class:"package-command",code {"{version.digest}"}}
                }
            }
            section { class: "panel mb-4", aria_label: "Package versions",
                h2 { class: "text-lg font-semibold p-4", "Versions" }
                if detail.versions.is_empty() { p { class: "p-4", "No versions published." } }
                for version in &detail.versions {
                    article { class: "package-row p-4",
                        div { h3 { class: "font-semibold", if version.deleted { "{version.name}" } else { NavLink { href: version.href.clone(), "{version.name}" } } }
                            p { class: "text-sm text-muted", "{version.created}" }
                            for tag in &version.tags { span { class: "badge", "{tag}" } }
                            if version.deleted { span { class: "badge", "Deleted" } }
                        }
                        if detail.settings {
                            form { method: "post", action: format!("{}/settings",detail.package.href),
                                {csrf(&detail.csrf_token)}
                                input { r#type: "hidden", name: "package_id", value: detail.package.id.to_string() }
                                input { r#type: "hidden", name: "version_id", value: version.id.to_string() }
                                input { r#type: "hidden", name: "action", value: if version.deleted {"restore-version"}else{"delete-version"} }
                                button { r#type: "submit", class: "btn", if version.deleted {"Restore"}else{"Delete version"} }
                            }
                        }
                    }
                }
            }
            {crate::webui::pages::discovery::page_links(detail.newer,detail.older,"Package version pages")}
        } },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_digests_wrap_at_narrow_widths() {
        assert!(include_str!("../components/native.css")
            .contains(".package-page h1, .package-page h2 { overflow-wrap: anywhere; }"));
    }

    #[test]
    fn packages_use_the_existing_responsive_repository_strip() {
        let mut repo = RepoChrome {
            owner: "alice".into(),
            name: "proj".into(),
            current_path: "/alice/proj/packages".into(),
            ..RepoChrome::default()
        };
        let html = dioxus_ssr::render_element(crate::webui::pages::repos::chrome::repo_tabs(&repo));
        assert!(!html.contains("/alice/proj/packages"));
        repo.packages = 12;
        let html = dioxus_ssr::render_element(crate::webui::pages::repos::chrome::repo_tabs(&repo));
        assert!(html.contains("repo-tabs-secondary"));
        assert!(html.contains("/alice/proj/packages"));
        assert!(html.contains("repo-tab-count\">12"));
        assert!(html.contains("repo-tabs-more-current"));
    }
}
