use crate::webui::pages::repos::{
    chrome::{chrome, icon, RepoChrome},
    commits::{latest_commit_bar, Commit},
    ref_path::{file_url, is_markdown_path, link_base},
    ref_switcher::{ref_options, RefOption, RefSwitcher},
    tree::{breadcrumbs, TreeEntry},
};
use crate::webui::{
    components::{
        code::{language_of_path, themed_highlight_lines},
        count_label, document_outline,
        references::References,
        CopyButton, OutlineEntry,
    },
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlobPage {
    pub reference: String,
    pub path: String,
    pub content: String,
    pub binary: bool,
    pub size: u64,
    pub offset: u64,
    pub next: Option<u64>,
    pub previous: Option<u64>,
    pub truncated: bool,
    /// Text small enough for the blame view. Set by the server, which owns the limit.
    #[serde(default)]
    pub can_blame: bool,
    pub editable: bool,
    /// The reference is a branch, so a commit can remove the file. Binary files are deletable too.
    #[serde(default)]
    pub deletable: bool,
    /// Shows a Markdown file as its source (`?view=code`) rather than rendered.
    #[serde(default)]
    pub code_view: bool,
    /// The references the Markdown links; empty unless `renders_markdown`.
    #[serde(default)]
    pub references: References,
    /// The root and each folder on the path to the file, root first, for the Files panel.
    #[serde(default)]
    pub folders: Vec<FolderListing>,
    /// The last commit that changed the file, shown above it.
    #[serde(default)]
    pub latest_commit: Option<Commit>,
    /// Every branch and every tag, sorted, for the ref menu.
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The file at the commit it was read at, so the link keeps showing this file. Empty when the
    /// server does not provide one.
    #[serde(default)]
    pub permalink: String,
}

/// The entries of one folder at the file's revision.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FolderListing {
    pub path: String,
    pub entries: Vec<TreeEntry>,
}

impl BlobPage {
    /// Whether the file has a rendered Markdown view, so the page offers Preview and Code.
    pub fn has_markdown_view(&self) -> bool {
        !self.binary && !self.truncated && is_markdown_path(&self.path)
    }

    /// Whether the view shows this file as Markdown rather than as code.
    pub fn renders_markdown(&self) -> bool {
        self.has_markdown_view() && !self.code_view
    }
}

pub fn blob(repo: &RepoChrome, page: &BlobPage) -> Element {
    let base = repo.base();
    let raw = file_url(&base, "raw", &page.reference, &page.path);
    let url = file_url(&base, "blob", &page.reference, &page.path);
    let blame = file_url(&base, "blame", &page.reference, &page.path);
    let edit = file_url(&base, "edit", &page.reference, &page.path);
    let delete = file_url(&base, "delete", &page.reference, &page.path);
    let name = page.path.rsplit('/').next().unwrap_or_default();
    let language = language_of_path(&page.path);
    let display = display_text(&page.content);
    let line_count = display.lines().count();
    // A preview that starts mid-file cannot say which line it starts at, so its lines get no
    // numbers and no #L anchors.
    let numbered = page.offset == 0;
    let line_numbers = if numbered {
        (1..=line_count)
            .map(|line| format!("<a href=\"#L{line}\">{line}</a>"))
            .collect::<String>()
    } else {
        String::new()
    };
    // Each line is its own block so the #L target can be highlighted, and the numbers line up
    // with the code without a newline between blocks.
    let code_lines = themed_highlight_lines(&display, language)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if numbered {
                format!(
                    "<span class=\"repo-code-line\" id=\"L{}\">{line}</span>",
                    index + 1
                )
            } else {
                format!("<span class=\"repo-code-line\">{line}</span>")
            }
        })
        .collect::<String>();
    let markdown = page.renders_markdown();
    let outline = if markdown {
        document_outline(&page.content, &page.references)
    } else {
        Vec::new()
    };
    let views = file_views(page, &url, &blame);
    let copy_label = if page.truncated {
        "Copy preview"
    } else if markdown {
        "Copy source"
    } else {
        "Copy code"
    };
    let stats = if page.binary {
        file_size(page.size)
    } else if page.truncated {
        format!(
            "{} · {}",
            count_label(line_count, "preview line", "preview lines"),
            file_size(page.size)
        )
    } else {
        file_stats(&display, page.size)
    };
    let card = if markdown {
        "repo-blob repo-blob-markdown border border-border rounded-md bg-card"
    } else {
        "repo-blob border border-border rounded-md bg-card"
    };
    let body = if markdown {
        rsx! {
            div { class: "repo-readme-body", crate::webui::components::RepoMarkdown { content: page.content.clone(), base: link_base(&base, &page.reference, page.path.rsplit_once('/').map_or("", |(directory, _)| directory)), references: page.references.clone() } }
        }
    } else if page.binary && is_inline_image(&page.path) {
        rsx! { div { class: "repo-image", img { src: raw.clone(), alt: "{name}" } } }
    } else if page.binary {
        rsx! { p { class: "repo-binary", "Binary file. Download it to view it." } }
    } else {
        rsx! {
            div { class: "repo-code-body overflow-x-auto font-mono text-sm",
                div { class: "repo-code-lines",
                    if !line_numbers.is_empty() { pre { class: "repo-line-numbers", dangerous_inner_html: line_numbers } }
                    pre { class: "shiki", code { dangerous_inner_html: code_lines } }
                }
            }
        }
    };
    let delete_link = (repo.can_write && page.deletable).then_some(delete);
    let content = rsx! {
        {title_row(repo, &page.reference, &page.path, true, rsx! {})}
        section { class: card,
            if let Some(commit) = &page.latest_commit {
                {latest_commit_bar(&base, &page.reference, &page.path, commit)}
            }
            div { class: "repo-blob-header",
                div { class: "repo-blob-heading",
                    {view_switch(&views)}
                    span { class: "repo-blob-stats", "{stats}" }
                }
                div { class: "repo-blob-actions",
                    {file_downloads(&raw, name, if page.binary { None } else { Some((page.content.as_str(), copy_label)) })}
                    if repo.can_write && page.editable {
                        NavLink { class: "repo-icon-button icon-tooltip".to_owned(), href: edit, aria_label: Some("Edit file".to_owned()), {icon("edit")} }
                    }
                    if outline.len() > 1 { {outline_menu(&outline)} }
                    {more_menu(&page.path, &page.permalink, delete_link)}
                }
            }
            {body}
        }
        if page.truncated { p { class: "text-sm text-muted-foreground", "Showing a bounded preview starting at byte {page.offset}. Raw contains the complete file." } }
        nav { class: "flex gap-4", "aria-label": "File preview",
            if let Some(offset) = page.previous { NavLink { href: format!("{url}?offset={offset}"), "Previous" } }
            if let Some(offset) = page.next { NavLink { href: format!("{url}?offset={offset}"), "Next" } }
        }
    };
    chrome(
        repo,
        file_layout(
            repo,
            &page.reference,
            &page.path,
            &page.folders,
            &page.branches,
            &page.tags,
            content,
        ),
    )
}

/// The layout every file page shares: the Files panel on the left and `content` beside it. The blob
/// and blame views both use it, so the Files panel and the file's column sit in the same place.
pub(in crate::webui::pages::repos) fn file_layout(
    repo: &RepoChrome,
    reference: &str,
    path: &str,
    folders: &[FolderListing],
    branches: &[String],
    tags: &[String],
    content: Element,
) -> Element {
    let base = repo.base();
    let directory = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    let branch_options = ref_options(&base, "blob", path, branches);
    let tag_options = ref_options(&base, "blob", path, tags);
    rsx! {
        div { class: "repo-blob-layout",
            input { r#type: "checkbox", id: "repo-files-toggle", class: "repo-sr-only repo-files-toggle" }
            FilesPanel { key: "{reference}/{path}", base: base.clone(), reference: reference.to_owned(), path: path.to_owned(), directory: directory.to_owned(), folders: folders.to_vec(), branches: branch_options, tags: tag_options, can_write: repo.can_write, default_branch: repo.default_branch.clone() }
            div { class: "repo-blob-main", {content} }
        }
    }
}

/// How many matches the Go to file dropdown lists. The last row of the dropdown opens every match.
const GO_TO_FILE_RESULTS: usize = 50;

/// The Files panel: the branch menu, the Add file menu, the search button, Go to file and the tree.
/// The folders on the way to `path` are open and the file is marked. `directory` is the folder new
/// files go into. The search button moves the reader to Go to file, which needs no script.
#[component]
pub fn FilesPanel(
    base: String,
    reference: String,
    path: String,
    directory: String,
    folders: Vec<FolderListing>,
    branches: Vec<RefOption>,
    tags: Vec<RefOption>,
    can_write: bool,
    default_branch: String,
) -> Element {
    rsx! {
        aside { class: "repo-files-panel", "aria-label": "Files",
            div { class: "repo-files-head",
                label { class: "repo-icon-button icon-tooltip", r#for: "repo-files-toggle", "aria-label": "Hide the file tree", {icon("files")} }
                span { class: "repo-files-title", "Files" }
            }
            div { class: "repo-files-tools",
                RefSwitcher { base: base.clone(), current: reference.clone(), branches: branches, tags: tags, default_branch: default_branch.clone() }
                if can_write {
                    {add_file_menu(&base, &reference, &directory, rsx! { summary { class: "repo-icon-button icon-tooltip", "aria-label": "Add file", {icon("plus")} } })}
                }
                label { class: "repo-icon-button icon-tooltip", r#for: "repo-files-filter", "aria-label": "Go to file", {icon("search")} }
            }
            GoToFile { base: base.clone(), reference: reference.clone(), folders: folders.clone() }
            div { class: "repo-files-tree", {folder_items(&base, &reference, &path, &folders, "")} }
        }
    }
}

/// Go to file: a box that finds the files and folders the page has loaded, by name. Once the page
/// runs in the browser, typing shows the matches in a dropdown, Enter opens the first one, Escape
/// closes the dropdown, and "/" anywhere on the page moves to the box. Without scripts the box
/// submits to the find page, which lists every file.
#[component]
pub fn GoToFile(base: String, reference: String, folders: Vec<FolderListing>) -> Element {
    let mut filter = use_signal(String::new);
    let mut open = use_signal(|| false);
    let typed = filter();
    let query = typed.trim().to_lowercase();
    let matches = matching_entries(&folders, &query);
    let showing = open() && !query.is_empty();
    let find_url = file_url(&base, "find", &reference, "");
    let all_matches = format!(
        "{find_url}?q={}",
        crate::webui::encode_component(typed.trim())
    );
    // Enter opens the first match once the page runs in the browser.
    let first = matches.first().map(|entry| {
        file_url(
            &base,
            if entry.kind == "tree" { "tree" } else { "blob" },
            &reference,
            &entry.path,
        )
    });
    rsx! {
        form { class: "repo-files-search", method: "get", action: find_url, role: "search",
            input { id: "repo-files-filter", class: "repo-files-filter", r#type: "search", name: "q", maxlength: "256", placeholder: "Go to file", "aria-label": "Go to file", "aria-keyshortcuts": "/", autocomplete: "off",
                oninput: move |event| {
                    filter.set(event.value());
                    open.set(true);
                },
                onfocus: move |_| {
                    if !filter.peek().trim().is_empty() {
                        open.set(true);
                    }
                },
                onblur: move |_| open.set(false),
                onkeydown: move |event| {
                    #[cfg(target_arch = "wasm32")]
                    {
                        if event.key() == Key::Escape {
                            open.set(false);
                        } else if event.key() == Key::Enter && showing {
                            if let Some(href) = &first {
                                event.prevent_default();
                                if let Some(window) = web_sys::window() {
                                    let _ = window.location().assign(href);
                                }
                            }
                        }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = (event, &first, showing);
                } }
            span { class: "repo-files-key", "aria-hidden": "true", "/" }
            if showing {
                div { class: "repo-files-results", onmousedown: move |event| event.prevent_default(),
                    if matches.is_empty() {
                        p { class: "repo-files-empty", "No loaded file has that name." }
                    } else {
                        ul { class: "repo-files-list repo-files-results-list",
                            for entry in matches.iter().take(GO_TO_FILE_RESULTS) {
                                li { key: "{entry.path}",
                                    NavLink { class: "repo-files-item".to_owned(), href: file_url(&base, if entry.kind == "tree" { "tree" } else { "blob" }, &reference, &entry.path),
                                        {icon(if entry.kind == "tree" { "folder" } else { "file" })}
                                        span { title: "{entry.path}", "{entry.path}" }
                                    }
                                }
                            }
                        }
                    }
                    NavLink { class: "repo-files-results-all".to_owned(), href: all_matches, "View all results" }
                }
            }
        }
    }
}

/// The loaded entries whose names contain `query`, sorted by path. `query` is already trimmed and
/// lower-cased, and an empty query matches nothing. Only the folders the page loaded are searched.
pub(in crate::webui::pages::repos) fn matching_entries<'a>(
    folders: &'a [FolderListing],
    query: &str,
) -> Vec<&'a TreeEntry> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<&TreeEntry> = folders
        .iter()
        .flat_map(|listing| listing.entries.iter())
        .filter(|entry| entry.name.to_lowercase().contains(query))
        .collect();
    matches.sort_by_key(|entry| entry.path.clone());
    matches
}

/// The Add file menu: the two ways to add a file to `directory`. The caller draws the summary, so the
/// Files panel and the repository toolbar each show their own button for the same menu.
pub(in crate::webui::pages::repos) fn add_file_menu(
    base: &str,
    reference: &str,
    directory: &str,
    summary: Element,
) -> Element {
    rsx! {
        details { class: "native-menu repo-menu repo-add-menu",
            {summary}
            div { class: "repo-menu-panel",
                NavLink { class: "repo-menu-item".to_owned(), href: file_url(base, "new", reference, directory), {icon("file")} "Create new file" }
                NavLink { class: "repo-menu-item".to_owned(), href: file_url(base, "upload", reference, directory), {icon("upload")} "Upload files" }
            }
        }
    }
}

/// The outline of a Markdown file: a menu of its headings. Each entry is a plain link to its
/// section, so the menu works without scripts. Callers draw it only for two or more headings.
pub(in crate::webui::pages::repos) fn outline_menu(outline: &[OutlineEntry]) -> Element {
    rsx! {
        details { class: "native-menu repo-menu repo-outline-menu",
            summary { class: "repo-icon-button icon-tooltip", "aria-label": "Outline", {icon("outline")} }
            div { class: "repo-menu-panel repo-outline-panel",
                nav { "aria-label": "Document outline",
                    for entry in outline {
                        a { key: "{entry.id}", class: "repo-menu-item", href: format!("#{}", entry.id), style: "--level: {entry.level}", "{entry.text}" }
                    }
                }
            }
        }
    }
}

/// The title row of a file or folder: the Files button for narrow screens, then the path with its
/// copy control, and `actions` at the right. `files_panel` says whether the page has a Files panel
/// to open.
pub(in crate::webui::pages::repos) fn title_row(
    repo: &RepoChrome,
    reference: &str,
    path: &str,
    files_panel: bool,
    actions: Element,
) -> Element {
    rsx! {
        div { class: "repo-blob-toolbar",
            div { class: "repo-blob-path",
                if files_panel {
                    label { class: "btn btn-secondary repo-files-open", r#for: "repo-files-toggle", title: "Show the file tree", {icon("files")} span { "Files" } }
                }
                {breadcrumbs(repo, reference, path, true)}
                if !path.is_empty() {
                    CopyButton { text: path.to_owned(), label: "Copy path".to_owned(), icon_only: true }
                }
            }
            div { class: "repo-file-actions", {actions} }
        }
    }
}

/// The "…" menu of a file or folder. It copies the path and the permalink and, when there is one,
/// offers the delete link below a divider. Without a permalink or a delete link it draws nothing.
pub(in crate::webui::pages::repos) fn more_menu(
    path: &str,
    permalink: &str,
    delete: Option<String>,
) -> Element {
    if permalink.is_empty() && delete.is_none() {
        return rsx! {};
    }
    rsx! {
        details { class: "native-menu repo-menu repo-more-menu",
            summary { class: "repo-icon-button icon-tooltip", "aria-label": "More actions", {icon("more")} }
            div { class: "repo-menu-panel",
                if !path.is_empty() {
                    CopyButton { text: path.to_owned(), label: "Copy path".to_owned(), menu_item: true }
                }
                if !permalink.is_empty() {
                    CopyButton { text: permalink.to_owned(), label: "Copy permalink".to_owned(), menu_item: true }
                }
                if let Some(href) = delete {
                    hr { class: "repo-menu-divider" }
                    NavLink { class: "repo-menu-item repo-menu-danger".to_owned(), href: href, {icon("trash")} "Delete file" }
                }
            }
        }
    }
}

/// One view of a file, as a segment of the view switch.
pub(in crate::webui::pages::repos) struct FileView {
    pub label: &'static str,
    pub href: String,
    pub active: bool,
}

/// The views of a file: Preview and Code for Markdown, Code otherwise, and Blame when the server
/// allows it. A binary file has no other view.
pub(in crate::webui::pages::repos) fn file_views(
    page: &BlobPage,
    url: &str,
    blame: &str,
) -> Vec<FileView> {
    let mut views = Vec::new();
    if page.has_markdown_view() {
        views.push(FileView {
            label: "Preview",
            href: url.to_owned(),
            active: page.renders_markdown(),
        });
        views.push(FileView {
            label: "Code",
            href: format!("{url}?view=code"),
            active: !page.renders_markdown(),
        });
    } else if !page.binary {
        views.push(FileView {
            label: "Code",
            href: url.to_owned(),
            active: true,
        });
    }
    if page.can_blame {
        views.push(FileView {
            label: "Blame",
            href: blame.to_owned(),
            active: false,
        });
    }
    views
}

/// The file's views as one segmented control. Plain links, so the switch works without JavaScript.
/// A single view has nothing to switch to, so no control is drawn.
pub(in crate::webui::pages::repos) fn view_switch(views: &[FileView]) -> Element {
    if views.len() < 2 {
        return rsx! {};
    }
    rsx! {
        nav { class: "repo-view-switch", "aria-label": "File view",
            for view in views {
                a { key: "{view.label}", class: if view.active { "repo-view-option repo-view-option-active" } else { "repo-view-option" }, href: view.href.clone(), aria_current: if view.active { "page" } else { "false" }, "{view.label}" }
            }
        }
    }
}

/// The file's Raw link, copy control and download link as one group. `copy` holds the text to
/// copy and its tooltip; a binary file has nothing to copy.
pub(in crate::webui::pages::repos) fn file_downloads(
    raw: &str,
    name: &str,
    copy: Option<(&str, &str)>,
) -> Element {
    rsx! {
        div { class: "repo-button-group",
            a { class: "btn btn-secondary", href: raw.to_owned(), "Raw" }
            if let Some((text, label)) = copy {
                CopyButton { text: text.to_owned(), label: label.to_owned(), icon_only: true }
            }
            a { class: "repo-icon-button icon-tooltip", href: raw.to_owned(), download: name.to_owned(), "aria-label": "Download", {icon("download")} }
        }
    }
}

/// The stats above a file in GitHub's words: "9 lines (8 loc) · 136 Bytes". A line is counted as
/// it is shown, and loc counts the lines that are not blank.
pub(in crate::webui::pages::repos) fn file_stats(text: &str, size: u64) -> String {
    let lines = text.lines().count();
    let loc = text.lines().filter(|line| !line.trim().is_empty()).count();
    format!(
        "{} ({}) · {}",
        count_label(lines, "line", "lines"),
        count_label(loc, "loc", "loc"),
        file_size(size)
    )
}

/// A size in GitHub's units: "136 Bytes", "3.68 KB", "1.20 MB".
fn file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    if bytes < 1024 {
        count_label(bytes as usize, "Byte", "Bytes")
    } else if (bytes as f64) < KB * KB {
        format!("{:.2} KB", bytes as f64 / KB)
    } else {
        format!("{:.2} MB", bytes as f64 / (KB * KB))
    }
}

/// The image types the raw route serves as images. SVG is served as text, so it is not drawn.
fn is_inline_image(path: &str) -> bool {
    let extension = path
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico"
    )
}

/// The rows of one folder's listing, folders first. A folder whose listing is loaded is a disclosure
/// that opens in place: the folders on the way to the page are loaded, and they start open. Any other
/// folder is a link to its own page, so every chevron does something.
fn folder_items(
    base: &str,
    reference: &str,
    current: &str,
    folders: &[FolderListing],
    folder: &str,
) -> Element {
    let Some(listing) = folders.iter().find(|listing| listing.path == folder) else {
        return rsx! {};
    };
    let mut entries: Vec<&TreeEntry> = listing.entries.iter().collect();
    entries.sort_by(|a, b| {
        (a.kind != "tree")
            .cmp(&(b.kind != "tree"))
            .then_with(|| a.name.cmp(&b.name))
    });
    rsx! {
        ul { class: "repo-files-list",
            for entry in entries {
                if entry.kind == "tree" && folders.iter().any(|loaded| loaded.path == entry.path) {
                    li { key: "{entry.path}",
                        details { class: "repo-files-folder", open: true,
                            summary { class: if entry.path == current { "repo-files-current" } else { "" },
                                span { class: "repo-files-chevron", {icon("chevron")} }
                                {icon("folder")} span { title: "{entry.name}", "{entry.name}" }
                            }
                            {folder_items(base, reference, current, folders, &entry.path)}
                        }
                    }
                } else if entry.kind == "tree" {
                    li { key: "{entry.path}",
                        NavLink { class: "repo-files-item".to_owned(), href: file_url(base, "tree", reference, &entry.path),
                            span { class: "repo-files-chevron", {icon("chevron")} }
                            {icon("folder")} span { title: "{entry.name}", "{entry.name}" }
                        }
                    }
                } else {
                    li { key: "{entry.path}",
                        NavLink { class: (if entry.path == current { "repo-files-item repo-files-current" } else { "repo-files-item" }).to_owned(), href: file_url(base, "blob", reference, &entry.path),
                            span { class: "repo-files-chevron-space", "aria-hidden": "true" }
                            {icon("file")} span { title: "{entry.name}", "{entry.name}" }
                        }
                    }
                }
            }
        }
    }
}

/// A carriage return starts a new line on screen, so the code view shows it as a line
/// break. Raw and copy views keep the file's own bytes.
pub(in crate::webui::pages::repos) fn display_text(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n")
}
