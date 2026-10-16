use super::{
    detail::{notice, pull_summary},
    review::{self, LineComment, Thread},
    Item, Kind, Repository,
};
use crate::webui::{
    components::{
        count_label, patch_badges, patch_gutter_digits, patch_rows, references::References,
        PatchRow,
    },
    encode_component,
    navigation::NavLink,
    pages::repositories,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiffFile {
    pub filename: String,
    pub status: String,
    pub additions: i32,
    pub deletions: i32,
    pub patch: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    error: Option<&str>,
    references: &References,
    split: bool,
) -> Element {
    let url = format!("{}/{}", repo.family_url(Kind::Pull), item.number);
    let head = item
        .comparison
        .as_ref()
        .map_or("", |compare| compare.head_sha.as_str());
    let lines = Lines {
        repo,
        url: &url,
        head,
        references,
        split,
    };
    // Reviews are sent from the conversation page, where the composer sits. Only a pull request
    // that accepts votes has one.
    let voting = item.state == "open"
        && item
            .comparison
            .as_ref()
            .is_some_and(|compare| compare.can_vote);
    // A conversation or composer on a file that is not in this diff has no row to sit under,
    // so it shows above the files.
    let listed = |path: &str| diff.files.iter().any(|file| file.filename == path);
    let unplaced: Vec<&Thread> = threads.iter().filter(|t| !listed(&t.path)).collect();
    // A reply draft is drawn inside its thread, so only a new line comment takes a composer here.
    let loose_composer =
        composer.filter(|draft| draft.in_reply_to.is_none() && !listed(&draft.path));
    repositories::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-diff-page",
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    {pull_summary(repo, item, Kind::Pull)}
                    nav { class: "collaboration-detail-tabs", aria_label: "Pull request",
                        NavLink { class: "collaboration-detail-tab", href: url.clone(), "Discussion" }
                        NavLink { class: "collaboration-detail-tab collaboration-detail-tab-active", href: format!("{url}/diff"), span { aria_current: "page", "Files changed" } }
                    }
                    repositories::TabStripScroll { strip: ".collaboration-detail-tabs", path: url.clone() }
                }
                {notice(error)}
                p { class: "collaboration-muted collaboration-diff-notice", "Comparing the current base and head branch tips." }
                if truncated {
                    p { class: "collaboration-panel collaboration-muted collaboration-diff-notice", "This pull request has more inline comments than the diff shows. The Discussion tab lists every comment." }
                }
                if let Some(draft) = loose_composer {
                    div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(repo, &url, head, draft, references, split)} }
                }
                for thread in unplaced {
                    {review::thread_block(repo, &url, head, thread, composer, references, split)}
                }
                div { class: "collaboration-diff-toolbar",
                    nav { class: "collaboration-view-toggle", aria_label: "Diff view",
                        NavLink { class: "collaboration-segment", href: format!("{url}/diff"), span { aria_current: if split { "false" } else { "page" }, "Unified" } }
                        NavLink { class: "collaboration-segment", href: format!("{url}/diff?view=split"), span { aria_current: if split { "page" } else { "false" }, "Split" } }
                    }
                    if voting {
                        NavLink { class: "btn btn-secondary", href: format!("{url}#review-form"), "Review changes" }
                    }
                }
                {files(Some(&lines), diff, threads, composer, diff.viewed.as_ref())}
            }
        },
    )
}

/// The Files changed list of a new pull request's preview and of a branch comparison: the same
/// rows, with no links, no conversations and no Viewed marks, because a comparison has no pull
/// request to keep them on.
pub(super) fn diff_view(diff: &Diff) -> Element {
    files(None, diff, &[], None, diff.viewed.as_ref())
}

/// The file list, then each file as a section that a reader can collapse. `viewed` holds the
/// Viewed marks of a signed-in reader; it is None where nobody can mark files.
fn files(
    lines: Option<&Lines<'_>>,
    diff: &Diff,
    threads: &[Thread],
    composer: Option<&LineComment>,
    viewed: Option<&Viewed>,
) -> Element {
    let file_noun = if diff.files.len() == 1 {
        "file"
    } else {
        "files"
    };
    rsx! {
        section { class: "collaboration-diff", aria_label: "Files changed",
            h3 { "{diff.files.len()} {file_noun} changed · " span { class: "collaboration-additions", "+{diff.additions}" } " " span { class: "collaboration-deletions", "−{diff.deletions}" } }
            if diff.files.is_empty() { p { class: "collaboration-panel collaboration-diff-empty collaboration-muted", "No file changes between these revisions." } }
            if !diff.files.is_empty() { {file_list(diff, viewed)} }
            for (index, file) in diff.files.iter().enumerate() {
                if let Some(marks) = viewed {
                    // The Viewed button sits in the file's header and submits this form by id.
                    form { id: format!("viewed-form-{index}"), class: "collaboration-viewed-form", method: "post", action: marks.action.clone(),
                        input { r#type: "hidden", name: "csrf_token", value: marks.csrf.clone() }
                        input { r#type: "hidden", name: "path", value: file.filename.clone() }
                    }
                }
                details { id: format!("diff-file-{index}"), open: !viewed.is_some_and(|marks| marks.paths.contains(&file.filename)), class: "collaboration-panel collaboration-diff-file",
                    summary { class: "collaboration-panel-header collaboration-diff-header",
                        span { class: "collaboration-diff-filename", "{file.filename}" }
                        for badge in patch_badges(&file.status, &file.patch) { span { class: "patch-badge", "{badge}" } }
                        span { class: "collaboration-additions", "+{file.additions}" }
                        span { class: "collaboration-deletions", "−{file.deletions}" }
                        if let Some(marks) = viewed {
                            ViewedBox {
                                csrf: marks.csrf.clone(),
                                action: marks.action.clone(),
                                path: file.filename.clone(),
                                form_id: format!("viewed-form-{index}"),
                                file_id: format!("diff-file-{index}"),
                                viewed: marks.paths.contains(&file.filename),
                            }
                        }
                    }
                    {file_body(lines, file, threads, composer)}
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
            ol { class: "collaboration-file-rows",
                for (index, file) in diff.files.iter().enumerate() {
                    li { class: "collaboration-file-row",
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
                    spawn(save_mark(action.clone(), csrf.clone(), path.clone(), next, marked, file_id.clone()));
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (&action, &csrf, &path);
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
    path: String,
    viewed: bool,
    mut marked: Signal<bool>,
    file_id: String,
) {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("csrf_token", &csrf)
        .append_pair("path", &path)
        .append_pair("viewed", if viewed { "yes" } else { "no" });
    let request = gloo_net::http::Request::post(&action)
        .header("Content-Type", "application/x-www-form-urlencoded")
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

/// The page-side work of the Viewed marks. Only the browser has a page, so off the browser the
/// functions do nothing.
mod browser {
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
) -> Element {
    if let Some(lines) = lines.filter(|lines| lines.split) {
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
                div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split)} }
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

/// A gutter number. A signed-in reader on a pull request gets a link that opens the composer on
/// that line.
fn gutter(lines: Option<&Lines<'_>>, path: &str, side: &str, number: Option<u32>) -> Element {
    match (number, lines) {
        (None, _) => rsx! { span { class: "patch-number" } },
        (Some(number), Some(lines)) if lines.repo.signed_in => {
            let view = if lines.split { "&view=split" } else { "" };
            rsx! {
                a { class: "patch-number", href: format!("{}/diff?path={}&side={side}&line={number}{view}#compose", lines.url, encode_component(path)), "{number}" }
            }
        }
        (Some(number), _) => rsx! { span { class: "patch-number", "{number}" } },
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
    // The gutter cells sit beside the text span, not inside it, so the span holds exactly the
    // line and copying a diff takes no numbers.
    rsx! {
        if numbered {
            div { class: "patch-row", "data-old": old, "data-new": new,
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
                div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split)} }
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
    lines: &Lines<'_>,
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
        for thread in top {
            {review::thread_block(lines.repo, lines.url, lines.head, thread, composer, lines.references, lines.split)}
        }
        if let Some(draft) = composer_top {
            div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split)} }
        }
        div { class: "collaboration-split patch-lines",
            for (index, pair) in pairs.iter().enumerate() {
                {split_row(lines, &file.filename, &rows, *pair)}
                for thread in under[index].clone() {
                    {review::thread_block(lines.repo, lines.url, lines.head, thread, composer, lines.references, lines.split)}
                }
                if composer_pair == Some(index) {
                    if let Some(draft) = here {
                        div { id: "compose", class: "collaboration-panel collaboration-diff-thread", {review::line_composer(lines.repo, lines.url, lines.head, draft, lines.references, lines.split)} }
                    }
                }
            }
        }
    }
}

/// One pair of a split patch. A line without numbers, such as a hunk header, spans both sides. A
/// context line shows on both sides; on a phone the stacked cells hide the mirrored right side.
fn split_row(
    lines: &Lines<'_>,
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
    lines: &Lines<'_>,
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
            {gutter(Some(lines), path, side, number)}
            span { class: row.class, "{row.text}" }
        }
    }
}
