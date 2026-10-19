//! Release pages: the list with its paging, a release's notes and downloads, and the form that
//! publishes a release or edits one, with its assets.
use crate::webui::components::{compact_date, confirm_dialog, format_bytes, Markdown};
use crate::webui::navigation::NavLink;
use crate::webui::pages::repos::chrome::{chrome, csrf, icon, RepoChrome};
use crate::webui::pages::repos::issues::editor::MarkdownEditor;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// A release as the pages show it. `title` is the release name, or the tag when it has none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReleaseItem {
    pub id: i64,
    pub tag: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub target: String,
    pub created_at: String,
    pub published_at: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub latest: bool,
}

/// An uploaded file of a release. `href` downloads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssetItem {
    pub id: i64,
    pub name: String,
    pub label: String,
    pub size: i64,
    pub downloads: i32,
    pub href: String,
}

/// What the release form holds: what the writer typed, or the release being edited.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ReleaseDraft {
    pub tag: String,
    pub target: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
    pub latest: bool,
    /// The Preview button was used, so the form opens on the rendered notes.
    pub preview: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    List {
        items: Vec<ReleaseItem>,
        total: i64,
        page: i64,
        per_page: i64,
        can_write: bool,
    },
    Detail {
        release: ReleaseItem,
        assets: Vec<AssetItem>,
        can_write: bool,
    },
    /// The new-release form (`editing` is `None`) or the edit form of a release.
    Form {
        editing: Option<i64>,
        draft: ReleaseDraft,
        /// The tags a new release can publish, offered as suggestions.
        tags: Vec<String>,
        branches: Vec<String>,
        default_branch: String,
        /// The release's assets, shown on its edit form.
        assets: Vec<AssetItem>,
        error: Option<String>,
    },
}

/// The release count and the latest tag, for the About column of the repository home.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReleasesSummary {
    pub count: i64,
    pub latest_tag: Option<String>,
}

/// The Releases block of the About column: the latest release and the others.
pub fn sidebar(repo: &RepoChrome, summary: &ReleasesSummary) -> Element {
    let base = repo.base();
    let others = summary.count - i64::from(summary.latest_tag.is_some());
    rsx! {
        div { class: "repo-about-releases",
            h2 { class: "repo-about-heading",
                NavLink { class: "repo-about-heading-link", href: format!("{base}/releases"), {icon("releases")} " Releases" }
                if summary.count > 0 { span { class: "count-badge", "{summary.count}" } }
            }
            if let Some(tag) = &summary.latest_tag {
                NavLink { class: "repo-about-release-latest", href: release_url(repo, tag), "{tag}" }
                span { class: "release-badge release-badge-latest", "Latest" }
            } else {
                span { class: "collaboration-muted", "No published releases" }
                if repo.can_write {
                    NavLink { class: "repo-about-release-count", href: format!("{base}/releases/new"), "Create a new release" }
                }
            }
            if others > 0 {
                NavLink { class: "repo-about-release-count", href: format!("{base}/releases"), "+ {others} releases" }
            } else if summary.latest_tag.is_some() {
                NavLink { class: "repo-about-release-count", href: format!("{base}/releases"), "All releases" }
            }
        }
    }
}

pub fn render(repo: &RepoChrome, page: Page) -> Element {
    let body = match page {
        Page::List {
            items,
            total,
            page,
            per_page,
            can_write,
        } => list(repo, &items, total, page, per_page, can_write),
        Page::Detail {
            release,
            assets,
            can_write,
        } => detail(repo, &release, &assets, can_write),
        Page::Form {
            editing,
            draft,
            tags,
            branches,
            default_branch,
            assets,
            error,
        } => form(
            repo,
            editing,
            &draft,
            &tags,
            &branches,
            &default_branch,
            &assets,
            error.as_deref(),
        ),
    };
    chrome(repo, body)
}

fn release_url(repo: &RepoChrome, tag: &str) -> String {
    format!(
        "{}/releases/tag/{}",
        repo.base(),
        crate::webui::encode_component(tag)
    )
}

/// The state badges of a release: latest, pre-release and draft.
fn badges(release: &ReleaseItem) -> Element {
    rsx! {
        if release.latest { span { class: "release-badge release-badge-latest", "Latest" } }
        if release.prerelease { span { class: "release-badge", "Pre-release" } }
        if release.draft { span { class: "release-badge release-badge-draft", "Draft" } }
    }
}

/// The date and author line under a release's title.
fn meta(release: &ReleaseItem) -> Element {
    let date = release
        .published_at
        .clone()
        .unwrap_or_else(|| release.created_at.clone());
    let verb = if release.published_at.is_some() {
        "published"
    } else {
        "drafted"
    };
    rsx! {
        span { class: "collaboration-muted",
            "{verb} "
            time { datetime: date.clone(), "{compact_date(&date)}" }
            " by {release.author}"
        }
    }
}

fn list(
    repo: &RepoChrome,
    items: &[ReleaseItem],
    total: i64,
    page: i64,
    per_page: i64,
    can_write: bool,
) -> Element {
    let base = repo.base();
    let last = (total + per_page - 1) / per_page.max(1);
    rsx! {
        section { class: "collaboration-page releases-page", aria_label: "Releases",
            header { class: "repo-page-heading",
                h2 { {icon("releases")} " Releases" }
                if can_write {
                    NavLink { class: "btn btn-primary btn-sm", href: format!("{base}/releases/new"), {icon("plus")} " New release" }
                }
            }
            {releases_tags_nav(repo, true)}
            if items.is_empty() {
                p { class: "collaboration-muted releases-empty",
                    "No releases yet. A release packages a tag with notes and files for people to download."
                }
            } else {
                ul { class: "releases-list",
                    for release in items {
                        li { class: "releases-row",
                            div { class: "releases-row-main",
                                NavLink { class: "releases-row-title", href: release_url(repo, &release.tag), "{release.title}" }
                                {badges(release)}
                                div { class: "releases-row-meta",
                                    span { class: "repo-ref-pill", {icon("tags")} "{release.tag}" }
                                    {meta(release)}
                                }
                            }
                        }
                    }
                }
            }
            if last > 1 {
                nav { class: "releases-pager", aria_label: "Release pages",
                    if page > 1 { NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/releases?page={}", page - 1), "Newer" } }
                    span { class: "collaboration-muted", "Page {page} of {last}" }
                    if page < last { NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/releases?page={}", page + 1), "Older" } }
                }
            }
        }
    }
}

/// The Releases | Tags switch shown on both pages, as on GitHub.
pub fn releases_tags_nav(repo: &RepoChrome, releases: bool) -> Element {
    let base = repo.base();
    rsx! {
        nav { class: "repo-ref-tabs", aria_label: "Releases and tags",
            NavLink { href: format!("{base}/releases"), class: if releases { "repo-ref-tab is-active" } else { "repo-ref-tab" }, {icon("releases")} " Releases" }
            NavLink { href: format!("{base}/tags"), class: if releases { "repo-ref-tab" } else { "repo-ref-tab is-active" }, {icon("tags")} " Tags" }
        }
    }
}

fn asset_row(asset: &AssetItem) -> Element {
    rsx! {
        li { class: "releases-asset",
            {icon("download")}
            a { class: "releases-asset-name", href: asset.href.clone(), "{asset.name}" }
            if !asset.label.is_empty() { span { class: "collaboration-muted", "{asset.label}" } }
            span { class: "collaboration-muted", "{format_bytes(asset.size)}" }
            span { class: "collaboration-muted", title: "Downloads", "{asset.downloads} downloads" }
        }
    }
}

fn detail(
    repo: &RepoChrome,
    release: &ReleaseItem,
    assets: &[AssetItem],
    can_write: bool,
) -> Element {
    let base = repo.base();
    // The archive names the tag's full ref, so a branch with the same name is not downloaded.
    let archive_ref = crate::webui::encode_component(&format!("refs/tags/{}", release.tag));
    let delete_id = format!("delete-release-{}", release.id);
    rsx! {
        section { class: "collaboration-page releases-page release-detail", aria_label: "Release {release.title}",
            header { class: "repo-page-heading",
                div {
                    NavLink { class: "collaboration-milestone-back", href: format!("{base}/releases"), "← Releases" }
                    h2 { "{release.title}" }
                    div { class: "releases-row-meta",
                        span { class: "repo-ref-pill", {icon("tags")} "{release.tag}" }
                        {badges(release)}
                        {meta(release)}
                    }
                }
                if can_write {
                    div { class: "releases-actions",
                        NavLink { class: "btn btn-secondary btn-sm", href: format!("{base}/releases/edit/{}", release.id), {icon("edit")} " Edit" }
                        a { class: "btn btn-danger btn-sm", href: format!("#{delete_id}"), {icon("trash")} " Delete" }
                    }
                }
            }
            div { class: "collaboration-markdown releases-notes",
                if release.body.trim().is_empty() {
                    p { class: "collaboration-muted", "No release notes." }
                } else {
                    Markdown { content: release.body.clone() }
                }
            }
            section { class: "releases-assets", aria_label: "Downloads",
                h3 { "Downloads" }
                ul { class: "releases-asset-list",
                    li { class: "releases-asset",
                        {icon("archive")}
                        a { class: "releases-asset-name", href: format!("{base}/archive/{archive_ref}.zip"), "Source code (zip)" }
                    }
                    li { class: "releases-asset",
                        {icon("archive")}
                        a { class: "releases-asset-name", href: format!("{base}/archive/{archive_ref}.tar.gz"), "Source code (tar.gz)" }
                    }
                    for asset in assets { {asset_row(asset)} }
                }
            }
            if can_write {
                {confirm_dialog(
                    Some(&delete_id),
                    "Delete release",
                    &release_url(repo, &release.tag),
                    &format!("{base}/releases/{}/delete", release.id),
                    rsx! { {csrf(&repo.csrf_token)} p { "Delete the release “{release.title}”? Its uploaded files are removed too. The tag stays." } },
                    "Delete release",
                )}
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "The form takes the page model's fields as they are."
)]
fn form(
    repo: &RepoChrome,
    editing: Option<i64>,
    draft: &ReleaseDraft,
    tags: &[String],
    branches: &[String],
    default_branch: &str,
    assets: &[AssetItem],
    error: Option<&str>,
) -> Element {
    let base = repo.base();
    let action = match editing {
        Some(id) => format!("{base}/releases/edit/{id}"),
        None => format!("{base}/releases/new"),
    };
    let title = if editing.is_some() {
        "Edit release"
    } else {
        "New release"
    };
    let branch_choices: Vec<String> = if branches.is_empty() {
        vec![default_branch.to_owned()]
    } else {
        branches.to_vec()
    };
    rsx! {
        section { class: "collaboration-page releases-page release-form", aria_label: "{title}",
            header { class: "repo-page-heading",
                h2 { "{title}" }
            }
            if let Some(error) = error {
                p { class: "collaboration-error", role: "alert", "{error}" }
            }
            form { method: "post", action: action.clone(), class: "releases-form", id: "release-form",
                {csrf(&repo.csrf_token)}
                input { r#type: "hidden", name: "repository_id", value: repo.id.to_string() }
                div { class: "collaboration-form-fields",
                    if editing.is_some() {
                        p { class: "releases-tag-fixed", {icon("tags")} " Tag " code { "{draft.tag}" } }
                    } else {
                        label { class: "collaboration-field", "Tag"
                            input { r#type: "text", name: "tag", class: "collaboration-input", required: true, maxlength: "255", list: "release-tag-choices", placeholder: "v1.2.0", value: draft.tag.clone() }
                            datalist { id: "release-tag-choices", for tag in tags { option { value: tag.clone() } } }
                            small { class: "collaboration-muted", "An existing tag is published as it is. A new tag is created at the target." }
                        }
                        label { class: "collaboration-field", "Target"
                            select { name: "target", class: "collaboration-input",
                                for branch in branch_choices {
                                    option { value: branch.clone(), selected: branch == draft.target || (draft.target.is_empty() && branch == default_branch), "{branch}" }
                                }
                            }
                        }
                    }
                    label { class: "collaboration-field", "Title"
                        input { r#type: "text", name: "title", class: "collaboration-input", maxlength: "255", placeholder: "Release title (defaults to the tag)", value: draft.title.clone() }
                    }
                    div { class: "collaboration-field", "Release notes"
                        MarkdownEditor { id: "release-body".to_owned(), initial: draft.body.clone(), rows: 12, required: false, initial_preview: draft.preview, placeholder: "Describe what changed".to_owned() }
                    }
                    div { class: "releases-flags",
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "prerelease", checked: draft.prerelease } " Pre-release" }
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "latest", checked: draft.latest } " Set as the latest release" }
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "draft", checked: draft.draft } " Save as draft (only writers see it)" }
                    }
                }
                div { class: "releases-form-actions",
                    a { class: "btn btn-secondary", href: format!("{base}/releases"), "Cancel" }
                    button { class: "btn btn-primary", r#type: "submit", name: "intent", value: "save", if draft.draft { "Save draft" } else if editing.is_some() { "Save release" } else { "Publish release" } }
                }
            }
            if let Some(id) = editing {
                section { class: "releases-assets", aria_label: "Assets",
                    h3 { "Assets" }
                    p { class: "collaboration-muted", "Files up to 100 MiB. Each name is used once per release." }
                    ul { class: "releases-asset-list",
                        for asset in assets {
                            li { class: "releases-asset",
                                {icon("download")}
                                a { class: "releases-asset-name", href: asset.href.clone(), "{asset.name}" }
                                span { class: "collaboration-muted", "{format_bytes(asset.size)}" }
                                span { class: "collaboration-muted", "{asset.downloads} downloads" }
                                a { class: "btn btn-danger btn-sm", href: format!("#delete-asset-{}", asset.id), {icon("trash")} " Remove" }
                            }
                        }
                    }
                    form { method: "post", action: format!("{base}/releases/{id}/assets"), enctype: "multipart/form-data", class: "releases-upload",
                        {csrf(&repo.csrf_token)}
                        input { r#type: "file", name: "file", required: true, class: "collaboration-input", aria_label: "Asset file" }
                        input { r#type: "text", name: "label", maxlength: "255", class: "collaboration-input", placeholder: "Label (optional)", aria_label: "Asset label" }
                        button { class: "btn btn-secondary", r#type: "submit", {icon("upload")} " Upload asset" }
                    }
                }
                for asset in assets {
                    {confirm_dialog(
                        Some(&format!("delete-asset-{}", asset.id)),
                        "Remove asset",
                        &format!("{base}/releases/edit/{id}"),
                        &format!("{base}/releases/{id}/assets/{}/delete", asset.id),
                        rsx! { {csrf(&repo.csrf_token)} p { "Remove “{asset.name}” from this release?" } },
                        "Remove asset",
                    )}
                }
            }
        }
    }
}
