use super::review::{self, LineComment, ReviewDraft, Thread};
use crate::webui::pages::repos::issues::{
    detail::{notice, pull_summary, pull_tabs, FilterBox, PickerRow, PullTab},
    Item, Kind, Repository,
};
use crate::webui::{
    components::{
        count_label, patch_badges, patch_gutter_digits, patch_rows, references::References,
        PatchRow,
    },
    encode_component,
    navigation::NavLink,
    pages::repos,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiffFile {
    pub filename: String,
    pub status: String,
    pub additions: i32,
    pub deletions: i32,
    pub patch: String,
    /// The owners the base branch's CODEOWNERS names for this file, as written.
    #[serde(default)]
    pub owners: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Diff {
    pub files: Vec<DiffFile>,
    pub additions: i32,
    pub deletions: i32,
    /// The Viewed marks of a signed-in reader on a pull request; None where nobody can mark files.
    pub viewed: Option<Viewed>,
}

/// What a signed-in reader needs to mark files on a pull request: the form's CSRF token, the
/// address the marks post to, and the paths whose mark matches the file's current contents.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Viewed {
    pub csrf: String,
    pub action: String,
    pub head: String,
    pub base: String,
    pub paths: std::collections::BTreeSet<String>,
}

/// What every line of the diff needs: where its forms post and which head they name.
struct Lines<'a> {
    repo: &'a Repository,
    url: &'a str,
    head: &'a str,
    references: &'a References,
    /// The side-by-side view is shown instead of the unified one.
    split: bool,
    /// The review a new line comment may start or join: None on a closed pull request, otherwise
    /// whether the viewer already has a pending review to add to.
    review: Option<bool>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each field of the diff page model is a parameter; a struct would only wrap them."
)]
pub fn diff_page(
    repo: &Repository,
    item: &Item,
    diff: &Diff,
    threads: &[Thread],
    truncated: bool,
    composer: Option<&LineComment>,
    review: Option<&ReviewDraft>,
    pending_comments: usize,
    error: Option<&str>,
    references: &References,
    split: bool,
) -> Element {
    let url = format!("{}/{}", repo.family_url(Kind::Pull), item.number);
    let head = item
        .comparison
        .as_ref()
        .map_or("", |compare| compare.head_sha.as_str());
    let open = item.state == "open";
    let lines = Lines {
        repo,
        url: &url,
        head,
        references,
        split,
        review: open.then_some(pending_comments > 0),
    };
    // Approving and requesting changes need a vote, and the author of a pull request cannot cast one
    // on it. The menu closes with the pull request, but keeps Discard review while comments wait.
    let vote = item
        .comparison
        .as_ref()
        .is_some_and(|compare| compare.can_vote);
    let author = !repo.viewer.is_empty() && item.author == repo.viewer;
    // A conversation or composer on a file that is not in this diff has no row to sit under,
    // so it shows above the files.
    let listed = |path: &str| diff.files.iter().any(|file| file.filename == path);
    let unplaced: Vec<&Thread> = threads.iter().filter(|t| !listed(&t.path)).collect();
    // A reply draft is drawn inside its thread, so only a new line comment takes a composer here.
    let loose_composer = composer.filter(|draft| match draft.in_reply_to {
        Some(root) => !threads.iter().any(|thread| thread.root.id == root),
        None => !listed(&draft.path),
    });
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-diff-page",
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    {pull_summary(repo, item, Kind::Pull)}
                    {pull_tabs(&url, item, PullTab::Files)}
                }
                {notice(error)}
                p { class: "collaboration-muted collaboration-diff-notice", "Comparing the current base and head branch tips." }
                if truncated {
                    p { class: "collaboration-panel collaboration-muted collaboration-diff-notice", "This pull request has more inline comments than the diff shows. The Discussion tab lists every comment." }
                }
                if let Some(draft) = loose_composer {
                    div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(repo, &url, head, draft, references, split, lines.review)} }
                }
                for thread in unplaced {
                    {review::thread_block(repo, &url, head, thread, composer, references, split)}
                }
                div { class: "collaboration-diff-toolbar",
                    {view_toggle(&format!("{url}/diff"), split)}
                    if repo.signed_in && (open || pending_comments > 0) {
                        {review::review_changes(repo, &url, head, open, author, vote, review, pending_comments, references)}
                    }
                }
                // A review refused after the pull request closed has no form to return to, so its text
                // is shown read-only here.
                if let (false, Some(refused)) = (open, review) {
                    {review::closed_review(refused)}
                }
                {files(Some(&lines), diff, threads, composer, diff.viewed.as_ref(), split)}
            }
        },
    )
}

/// The Files changed list of a new pull request's preview, of a branch comparison and of a commit:
/// the same rows, with no links, no conversations and no Viewed marks, because none of those has a
/// pull request to keep them on.
pub(in crate::webui::pages::repos) fn diff_view(diff: &Diff, split: bool) -> Element {
    files(None, diff, &[], None, diff.viewed.as_ref(), split)
}

/// The Unified and Split buttons above a diff that has no pull request. Each links to `url` with
/// the view named, so the choice is an ordinary address.
pub(in crate::webui::pages::repos) fn view_toggle(url: &str, split: bool) -> Element {
    let join = if url.contains('?') { '&' } else { '?' };
    rsx! {
        nav { class: "collaboration-view-toggle", aria_label: "Diff view",
            NavLink { class: "collaboration-segment", href: url.to_owned(), span { aria_current: if split { "false" } else { "page" }, "Unified" } }
            NavLink { class: "collaboration-segment", href: format!("{url}{join}view=split"), span { aria_current: if split { "page" } else { "false" }, "Split" } }
        }
    }
}

/// The count of changed files and of added and removed lines, the heading of the Files changed list.
pub(in crate::webui::pages::repos) fn diff_summary(diff: &Diff) -> Element {
    let file_noun = if diff.files.len() == 1 {
        "file"
    } else {
        "files"
    };
    rsx! {
        h3 { {repos::chrome::icon("file")} "{diff.files.len()} {file_noun} changed · " span { class: "collaboration-additions", "+{diff.additions}" } " " span { class: "collaboration-deletions", "−{diff.deletions}" } }
    }
}

/// The summary line, then the file tree beside each file as a section that a reader can collapse.
/// `viewed` holds the Viewed marks of a signed-in reader; it is None where nobody can mark files.
fn files(
    lines: Option<&Lines<'_>>,
    diff: &Diff,
    threads: &[Thread],
    composer: Option<&LineComment>,
    viewed: Option<&Viewed>,
    split: bool,
) -> Element {
    rsx! {
        section { class: "collaboration-diff", aria_label: "Files changed",
            div { class: "collaboration-diff-summary",
                {diff_summary(diff)}
                if !diff.files.is_empty() { FileToggles { count: diff.files.len() } }
            }
            if diff.files.is_empty() { p { class: "collaboration-panel collaboration-diff-empty collaboration-muted", "No file changes." } }
            if !diff.files.is_empty() {
                div { class: "collaboration-diff-layout",
                    // The toggle is the tree's sibling, not its parent. On a phone the tree is hidden while
                    // the toggle is open, and a native disclosure cannot hide content that it itself holds.
                    div { class: "collaboration-diff-tree-column",
                        details { class: "collaboration-diff-tree-toggle", open: true,
                            summary { {repos::chrome::icon("files")} "Files" }
                        }
                        aside { class: "collaboration-diff-tree", {file_list(diff, viewed)} }
                    }
                    div { class: "collaboration-diff-files", {file_cards(lines, diff, threads, composer, viewed, split)} }
                }
            }
        }
    }
}

/// The buttons that open or close every file at once. They appear once the page runs, so without
/// JavaScript the page does not show a control that cannot work.
#[component]
fn FileToggles(count: usize) -> Element {
    let mut enhanced = use_signal(|| false);
    use_effect(move || enhanced.set(true));
    rsx! {
        if enhanced() {
            div { class: "collaboration-file-toggles",
                button { r#type: "button", class: "repo-icon-button", title: "Expand all files", aria_label: "Expand all files", onclick: move |_| browser::set_all(count, true), {repos::chrome::icon("expand")} }
                button { r#type: "button", class: "repo-icon-button", title: "Collapse all files", aria_label: "Collapse all files", onclick: move |_| browser::set_all(count, false), {repos::chrome::icon("collapse")} }
            }
        }
    }
}

const MAX_DIFF_ROWS: usize = 4000;
const MAX_DIFF_HTML_BYTES: usize = 8 * 1024 * 1024;

// A line repeats its encoded path in up to four links in split view. Reserve room for
// escaped text and row markup too, before constructing any of the row components.
fn renderable_files(diff: &Diff) -> Vec<bool> {
    let mut rows_left = MAX_DIFF_ROWS;
    let mut bytes_left = MAX_DIFF_HTML_BYTES;
    diff.files
        .iter()
        .map(|file| {
            let rows = file.patch.lines().count();
            let bytes = file.patch.len().saturating_mul(6).saturating_add(
                rows.saturating_mul(2048 + 4 * encode_component(&file.filename).len()),
            );
            if rows > rows_left || bytes > bytes_left {
                false
            } else {
                rows_left -= rows;
                bytes_left -= bytes;
                true
            }
        })
        .collect()
}

/// Each file as a section that a reader can collapse. The Viewed mark sits in the file's header.
fn file_cards(
    lines: Option<&Lines<'_>>,
    diff: &Diff,
    threads: &[Thread],
    composer: Option<&LineComment>,
    viewed: Option<&Viewed>,
    split: bool,
) -> Element {
    let renderable = renderable_files(diff);
    rsx! {
        for (index, file) in diff.files.iter().enumerate() {
            if let Some(marks) = viewed {
                // The Viewed button sits in the file's header and submits this form by id.
                form { id: format!("viewed-form-{index}"), class: "collaboration-viewed-form", method: "post", action: marks.action.clone(),
                    input { r#type: "hidden", name: "csrf_token", value: marks.csrf.clone() }
                    input { r#type: "hidden", name: "path", value: file.filename.clone() }
                    input { r#type: "hidden", name: "expected_head", value: marks.head.clone() }
                    input { r#type: "hidden", name: "expected_base", value: marks.base.clone() }
                    input { r#type: "hidden", name: "index", value: index.to_string() }
                }
            }
            details { id: format!("diff-file-{index}"), open: !viewed.is_some_and(|marks| marks.paths.contains(&file.filename)), class: "collaboration-panel collaboration-diff-file",
                summary { class: "collaboration-panel-header collaboration-diff-header",
                    span { class: "collaboration-diff-filename", "{file.filename}" }
                    if !file.owners.is_empty() {
                        span { class: "collaboration-badge", title: "Code owners of this file", {format!("Owned by {}", file.owners.join(", "))} }
                    }
                    for badge in patch_badges(&file.status, &file.patch) { span { class: "patch-badge", "{badge}" } }
                    span { class: "collaboration-additions", "+{file.additions}" }
                    span { class: "collaboration-deletions", "−{file.deletions}" }
                    if let Some(marks) = viewed {
                        ViewedBox {
                            csrf: marks.csrf.clone(),
                            action: marks.action.clone(),
                            path: file.filename.clone(),
                            head: marks.head.clone(),
                            base: marks.base.clone(),
                            form_id: format!("viewed-form-{index}"),
                            file_id: format!("diff-file-{index}"),
                            viewed: marks.paths.contains(&file.filename),
                        }
                    }
                }
                if renderable[index] {
                    {file_body(lines, file, threads, composer, split)}
                } else {
                    p { class: "collaboration-diff-notice", role: "status", "This patch is too large to display. Use git diff locally to view the complete changes." }
                }
            }
        }
    }
}

/// One row per changed file, with its status and counts. The path links to the file's section.
/// With `viewed` set, the header counts the files marked as viewed.
fn file_list(diff: &Diff, viewed: Option<&Viewed>) -> Element {
    let total = diff.files.len();
    let done = viewed.map_or(0, |marks| marks.paths.len());
    rsx! {
        nav { class: "collaboration-file-list", aria_label: "Changed files",
            if viewed.is_some() {
                p { class: "collaboration-viewed-count collaboration-muted",
                    span { class: "collaboration-viewed-done", "{done}" }
                    " / " {count_label(total, "file", "files")} " viewed"
                }
            }
            FilterBox { placeholder: "Filter files",
                ol { class: "collaboration-file-rows",
                    for (index, file) in diff.files.iter().enumerate() {
                        PickerRow { key: "{file.filename}", text: file.filename.clone(), class: "collaboration-file-row",
                            span { class: format!("collaboration-file-status collaboration-file-{}", file_status_class(&file.status)), "{file_status_label(&file.status)}" }
                            a { class: "collaboration-file-path", href: format!("#diff-file-{index}"), "{file.filename}" }
                            span { class: "collaboration-file-counts",
                                span { class: "collaboration-additions", "+{file.additions}" }
                                span { class: "collaboration-deletions", "−{file.deletions}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn file_status_label(status: &str) -> &'static str {
    match status {
        "added" => "Added",
        "removed" => "Deleted",
        "renamed" => "Renamed",
        _ => "Modified",
    }
}

fn file_status_class(status: &str) -> &'static str {
    match status {
        "added" => "added",
        "removed" => "deleted",
        "renamed" => "renamed",
        _ => "modified",
    }
}

/// The Viewed button in a file's header. Without scripts it submits its form, which redirects back
/// to the file. After hydration the mark is drawn at once, the file opens or collapses, and the
/// post goes in the background; a post that fails undoes the change.
#[component]
fn ViewedBox(
    csrf: String,
    action: String,
    path: String,
    head: String,
    base: String,
    form_id: String,
    file_id: String,
    viewed: bool,
) -> Element {
    let mut marked = use_signal(|| viewed);
    rsx! {
        button { r#type: "submit", class: "collaboration-viewed", role: "checkbox", form: form_id,
            name: "viewed", value: if marked() { "no" } else { "yes" },
            aria_checked: if marked() { "true" } else { "false" },
            onclick: move |event| {
                event.prevent_default();
                let next = !marked();
                marked.set(next);
                browser::set_mark(&file_id, next);
                #[cfg(target_arch = "wasm32")]
                {
                    spawn(save_mark(action.clone(), csrf.clone(), (path.clone(), head.clone(), base.clone()), next, marked, file_id.clone()));
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (&action, &csrf, &path, &head, &base);
            },
            "Viewed"
        }
    }
}

/// Posts a file's mark in the background. A post that fails puts the button and the file back.
#[cfg(target_arch = "wasm32")]
async fn save_mark(
    action: String,
    csrf: String,
    (path, head, base): (String, String, String),
    viewed: bool,
    mut marked: Signal<bool>,
    file_id: String,
) {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("csrf_token", &csrf)
        .append_pair("path", &path)
        .append_pair("expected_head", &head)
        .append_pair("expected_base", &base)
        .append_pair("background", "yes")
        .append_pair("viewed", if viewed { "yes" } else { "no" });
    let request = gloo_net::http::Request::post(&action)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .body(serializer.finish());
    let saved = match request {
        Ok(request) => request.send().await.is_ok_and(|response| response.ok()),
        Err(_) => false,
    };
    if !saved {
        marked.set(!viewed);
        browser::set_mark(&file_id, !viewed);
    }
}

/// The page-side work of the file sections: the Viewed marks and the buttons that open or close
/// every file. Only the browser has a page, so off the browser the functions do nothing.
mod browser {
    /// Opens or closes every file's section. The sections carry the ids `diff-file-{index}`.
    pub fn set_all(count: usize, open: bool) {
        #[cfg(target_arch = "wasm32")]
        {
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            for index in 0..count {
                if let Some(details) = document.get_element_by_id(&format!("diff-file-{index}")) {
                    let _ = details.toggle_attribute_with_force("open", open);
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (count, open);
    }

    /// Draws a file's mark: its button shows the state, its section opens unless it is viewed,
    /// and the count of its diff follows.
    pub fn set_mark(id: &str, viewed: bool) {
        #[cfg(target_arch = "wasm32")]
        {
            let Some(details) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id(id))
            else {
                return;
            };
            let _ = details.toggle_attribute_with_force("open", !viewed);
            if let Ok(Some(button)) = details.query_selector(".collaboration-viewed") {
                let _ = button.set_attribute("aria-checked", if viewed { "true" } else { "false" });
            }
            refresh_count(&details);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (id, viewed);
    }

    /// Writes the number of viewed files into this diff's list header.
    #[cfg(target_arch = "wasm32")]
    fn refresh_count(details: &web_sys::Element) {
        let Ok(Some(section)) = details.closest(".collaboration-diff") else {
            return;
        };
        let checked = section
            .query_selector_all(".collaboration-viewed[aria-checked=\"true\"]")
            .map(|list| list.length())
            .unwrap_or(0);
        if let Ok(Some(done)) = section.query_selector(".collaboration-viewed-done") {
            done.set_text_content(Some(&checked.to_string()));
        }
    }
}

/// The row a thread or composer sits under: the first row that shows its line on its side.
fn row_index(rows: &[PatchRow<'_>], side: &str, line: i32) -> Option<usize> {
    let number = u32::try_from(line).ok()?;
    rows.iter().position(|row| {
        if side == "LEFT" {
            row.old == Some(number)
        } else {
            row.new == Some(number)
        }
    })
}

/// One file's patch. A conversation whose line is no longer in the patch shows above it.
fn file_body(
    lines: Option<&Lines<'_>>,
    file: &DiffFile,
    threads: &[Thread],
    composer: Option<&LineComment>,
    split: bool,
) -> Element {
    if split {
        return split_file_body(lines, file, threads, composer);
    }
    let rows = patch_rows(&file.patch);
    let mut under: Vec<Vec<&Thread>> = vec![Vec::new(); rows.len()];
    let mut top: Vec<&Thread> = Vec::new();
    for thread in threads.iter().filter(|thread| thread.path == file.filename) {
        match thread
            .line
            .and_then(|line| row_index(&rows, &thread.side, line))
        {
            Some(index) => under[index].push(thread),
            None => top.push(thread),
        }
    }
    let here = composer.filter(|draft| draft.in_reply_to.is_none() && draft.path == file.filename);
    let composer_index =
        here.and_then(|draft| row_index(&rows, &draft.side, draft.line.parse().ok()?));
    let composer_top = here.filter(|_| composer_index.is_none());
    let digits = patch_gutter_digits(&rows);
    rsx! {
        if let Some(lines) = lines {
            for thread in top {
                {review::thread_block(lines.repo, lines.url, lines.head, thread, composer, lines.references, lines.split)}
            }
            if let Some(draft) = composer_top {
                div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split, lines.review)} }
            }
        }
        pre { class: "collaboration-diff-patch",
            code { class: "patch-lines", "data-digits": digits,
                // Header rows have no line number, so no conversation sits under them; the index
                // still addresses `under`, which is built from every row.
                for (index, row) in rows.iter().enumerate().filter(|(_, row)| !row.header) {
                    {patch_row(lines, &file.filename, row, &under[index], composer, if composer_index == Some(index) { here } else { None })}
                }
            }
        }
    }
}

/// Where the composer for one line opens: the diff page with that line named. Only a signed-in
/// reader on a pull request gets one.
fn composer_href(lines: Option<&Lines<'_>>, path: &str, side: &str, number: u32) -> Option<String> {
    let lines = lines.filter(|lines| lines.repo.signed_in)?;
    let view = if lines.split { "&view=split" } else { "" };
    Some(format!(
        "{}/diff?path={}&side={side}&line={number}{view}#compose",
        lines.url,
        encode_component(path)
    ))
}

/// A gutter number. A reader who can comment gets a link that opens the composer on that line.
fn gutter(lines: Option<&Lines<'_>>, path: &str, side: &str, number: Option<u32>) -> Element {
    let Some(number) = number else {
        return rsx! { span { class: "patch-number" } };
    };
    match composer_href(lines, path, side, number) {
        Some(href) => rsx! { a { class: "patch-number", href: href, "{number}" } },
        None => rsx! { span { class: "patch-number", "{number}" } },
    }
}

/// The "+" that opens the composer on a line. It is always drawn so the columns line up; a reader
/// who cannot comment gets an empty cell.
fn comment_cell(lines: Option<&Lines<'_>>, path: &str, side: &str, number: Option<u32>) -> Element {
    let target = number
        .and_then(|number| composer_href(lines, path, side, number).map(|href| (number, href)));
    match target {
        Some((number, href)) => rsx! {
            a { class: "patch-add", href: href, title: "Comment on line {number}", aria_label: "Comment on line {number}",
                {repos::chrome::icon("plus")}
            }
        },
        None => rsx! { span { class: "patch-add" } },
    }
}

/// One line, then the conversations under it. `reply` is the page's composer, which reopens its
/// thread's reply form wherever that thread is; `line_draft` is the composer for this line.
fn patch_row(
    lines: Option<&Lines<'_>>,
    path: &str,
    row: &PatchRow<'_>,
    threads: &[&Thread],
    reply: Option<&LineComment>,
    line_draft: Option<&LineComment>,
) -> Element {
    let old = row.old.map(|number| number.to_string());
    let new = row.new.map(|number| number.to_string());
    let numbered = row.old.is_some() || row.new.is_some();
    // A removed line is commented on at its old number; every other line at its new one.
    let (add_side, add_number) = if row.new.is_some() {
        ("RIGHT", row.new)
    } else {
        ("LEFT", row.old)
    };
    // The gutter cells sit beside the text span, not inside it, so the span holds exactly the
    // line and copying a diff takes no numbers.
    rsx! {
        if numbered {
            div { class: "patch-row", "data-old": old, "data-new": new,
                {comment_cell(lines, path, add_side, add_number)}
                {gutter(lines, path, "LEFT", row.old)}
                {gutter(lines, path, "RIGHT", row.new)}
                span { class: row.class, "{row.text}" }
            }
        } else {
            span { class: row.class, "{row.text}" }
        }
        if let Some(lines) = lines {
            for thread in threads {
                {review::thread_block(lines.repo, lines.url, lines.head, thread, reply, lines.references, lines.split)}
            }
            if let Some(draft) = line_draft {
                div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split, lines.review)} }
            }
        }
    }
}

/// The rows of a split patch, as pairs of row indexes: the removed line on the left and the added
/// line on the right. A run of removed lines pairs with the run of added lines after it, in order.
/// A context line or a header takes both sides, so one index sits in both places.
fn split_pairs(rows: &[PatchRow<'_>]) -> Vec<(Option<usize>, Option<usize>)> {
    fn flush(
        pairs: &mut Vec<(Option<usize>, Option<usize>)>,
        removed: &mut Vec<usize>,
        added: &mut Vec<usize>,
    ) {
        for step in 0..removed.len().max(added.len()) {
            pairs.push((removed.get(step).copied(), added.get(step).copied()));
        }
        removed.clear();
        added.clear();
    }
    let mut pairs = Vec::new();
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        // Git's header lines are not drawn; the file's header row names the file.
        if row.header {
            continue;
        }
        if row.old.is_some() && row.new.is_none() {
            removed.push(index);
        } else if row.new.is_some() && row.old.is_none() {
            added.push(index);
        } else {
            flush(&mut pairs, &mut removed, &mut added);
            pairs.push((Some(index), Some(index)));
        }
    }
    flush(&mut pairs, &mut removed, &mut added);
    pairs
}

/// One file's patch side by side. A conversation or composer sits under the pair of its line, on
/// the side it was written for.
fn split_file_body(
    lines: Option<&Lines<'_>>,
    file: &DiffFile,
    threads: &[Thread],
    composer: Option<&LineComment>,
) -> Element {
    let rows = patch_rows(&file.patch);
    let pairs = split_pairs(&rows);
    let mut pair_of = vec![0; rows.len()];
    for (index, pair) in pairs.iter().enumerate() {
        for row in [pair.0, pair.1].into_iter().flatten() {
            pair_of[row] = index;
        }
    }
    let mut under: Vec<Vec<&Thread>> = vec![Vec::new(); pairs.len()];
    let mut top: Vec<&Thread> = Vec::new();
    for thread in threads.iter().filter(|thread| thread.path == file.filename) {
        match thread
            .line
            .and_then(|line| row_index(&rows, &thread.side, line))
        {
            Some(row) => under[pair_of[row]].push(thread),
            None => top.push(thread),
        }
    }
    let here = composer.filter(|draft| draft.in_reply_to.is_none() && draft.path == file.filename);
    let composer_row =
        here.and_then(|draft| row_index(&rows, &draft.side, draft.line.parse().ok()?));
    let composer_top = here.filter(|_| composer_row.is_none());
    let composer_pair = composer_row.map(|row| pair_of[row]);
    rsx! {
        if let Some(lines) = lines {
            for thread in top {
                {review::thread_block(lines.repo, lines.url, lines.head, thread, composer, lines.references, lines.split)}
            }
            if let Some(draft) = composer_top {
                div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split, lines.review)} }
            }
        }
        div { class: "collaboration-split patch-lines",
            for (index, pair) in pairs.iter().enumerate() {
                {split_row(lines, &file.filename, &rows, *pair)}
                if let Some(lines) = lines {
                    for thread in under[index].clone() {
                        {review::thread_block(lines.repo, lines.url, lines.head, thread, composer, lines.references, lines.split)}
                    }
                    if composer_pair == Some(index) {
                        if let Some(draft) = here {
                            div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split, lines.review)} }
                        }
                    }
                }
            }
        }
    }
}

/// One pair of a split patch. A line without numbers, such as a hunk header, spans both sides. A
/// context line shows on both sides; on a phone the stacked cells hide the mirrored right side.
fn split_row(
    lines: Option<&Lines<'_>>,
    path: &str,
    rows: &[PatchRow<'_>],
    pair: (Option<usize>, Option<usize>),
) -> Element {
    let left = pair.0.map(|index| &rows[index]);
    if let Some(row) = left.filter(|row| row.old.is_none() && row.new.is_none()) {
        return rsx! {
            div { class: "collaboration-split-row collaboration-split-wide",
                span { class: row.class, "{row.text}" }
            }
        };
    }
    let right = pair.1.map(|index| &rows[index]);
    let mirrored = pair.0 == pair.1;
    rsx! {
        div { class: "collaboration-split-row",
            {split_cell(lines, path, "LEFT", left, false)}
            {split_cell(lines, path, "RIGHT", right, mirrored)}
        }
    }
}

/// One side of a split row. An empty side is a blank cell, so the other side keeps its column.
fn split_cell(
    lines: Option<&Lines<'_>>,
    path: &str,
    side: &str,
    row: Option<&PatchRow<'_>>,
    mirrored: bool,
) -> Element {
    let Some(row) = row else {
        return rsx! { div { class: "collaboration-split-cell collaboration-split-empty" } };
    };
    let number = if side == "LEFT" { row.old } else { row.new };
    let class = if mirrored {
        "collaboration-split-cell collaboration-split-mirror"
    } else {
        "collaboration-split-cell"
    };
    rsx! {
        div { class,
            {comment_cell(lines, path, side, number)}
            {gutter(lines, path, side, number)}
            span { class: row.class, "{row.text}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blank_line_patches_and_long_paths_cannot_expand_without_limit() {
        let file = |filename: String, patch: String| DiffFile {
            filename,
            patch,
            status: "added".into(),
            additions: 0,
            deletions: 0,
            owners: vec![],
        };
        let diff = Diff {
            files: vec![
                file("a".repeat(3072), "+\n".repeat(100_000)),
                file("small".into(), "@@ -0,0 +1 @@\n+ok".into()),
            ],
            ..Default::default()
        };
        assert_eq!(renderable_files(&diff), vec![false, true]);
        let html =
            crate::webui::tests::in_scope(|| dioxus_ssr::render_element(diff_view(&diff, false)));
        assert!(html.contains("This patch is too large to display"));
        assert!(html.len() < 100_000);
        let diff = Diff {
            files: (0..10)
                .map(|i| file(i.to_string(), "+\n".repeat(1000)))
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            renderable_files(&diff)
                .iter()
                .filter(|shown| **shown)
                .count(),
            4
        );
    }
}
