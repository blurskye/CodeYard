mod regressions;

use super::editor::MarkdownEditor;
use super::list::empty_title;
use super::*;
use crate::webui::components::references::References;
use crate::webui::pages::repos::actions::Run;
use crate::webui::pages::repos::chrome::RepoChrome;
use crate::webui::pages::repos::commits::Commit;
use crate::webui::pages::repos::compare::compare;
use crate::webui::pages::repos::pulls::diff::diff_view;
use crate::webui::pages::repos::pulls::{
    checks_page, commits_page, diff_page, Conversation, Diff, DiffFile, LineComment, PullSummary,
    Review, ReviewDraft, Reviewer, ReviewerStatus, Thread, Viewed,
};
use crate::webui::pages::repos::{self, ref_path::file_url};
use crate::webui::pages::repos::{labels, milestones};
use dioxus::prelude::*;

#[test]
fn new_issue_textareas_have_repository_and_template_draft_identity() {
    for form in [
        Vec::new(),
        vec![new::FormField {
            kind: new::FieldKind::Textarea,
            label: "Details".into(),
            ..Default::default()
        }],
    ] {
        let draft = NewDraft {
            repository_id: "42".into(),
            template: ".github/ISSUE_TEMPLATE/bug.yml".into(),
            template_revision: "template-A".into(),
            form,
            ..Default::default()
        };
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(new::new(
                &repository(),
                Kind::Issue,
                &draft,
                &BranchChoices::default(),
                &IssueChoices::default(),
                None,
                None,
                false,
                &References::default(),
            ))
        });
        assert!(html.contains("name=\"repository_id\" value=\"42\""));
        assert!(html.contains("name=\"template\" value=\".github/ISSUE_TEMPLATE/bug.yml\""));
        assert!(html.contains("name=\"template_revision\" value=\"template-A\""));
        assert!(html.contains("<textarea"));
    }
}

#[test]
fn refused_orphaned_replies_keep_their_text_in_a_recovery_composer() {
    let draft = LineComment {
        in_reply_to: Some(999),
        body: "keep this orphaned reply".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(diff_page(
        &repository(),
        &item(),
        &Diff::default(),
        &[],
        false,
        Some(&draft),
        None,
        0,
        Some("Parent comment not found"),
        &References::default(),
        false,
    ));
    assert!(html.contains("keep this orphaned reply"));
    assert!(html.contains("name=\"in_reply_to\" value=\"999\""));
}

#[test]
fn label_url_changes_do_not_copy_the_available_catalogues() {
    let filters = ListFilters {
        labels: vec![Label {
            id: 1,
            name: "bug".into(),
            color: "ff0000".into(),
            description: "large description".repeat(100),
        }],
        authors: vec!["alice".into()],
        assignees: vec!["bob".into()],
        selected_labels: vec!["chosen".into()],
        author: "alice".into(),
        ..Default::default()
    };
    let next = super::list::toggle_label(&filters, "bug");
    assert!(next.labels.is_empty() && next.authors.is_empty() && next.assignees.is_empty());
    assert_eq!(next.selected_labels, vec!["chosen", "bug"]);
    assert_eq!(next.author, "alice");
}

/// The list page without filter choices, which most of these tests do not render.
fn list(
    repo: &Repository,
    kind: Kind,
    state: &str,
    query: &str,
    counts: ListCounts,
    items: &[Item],
) -> Element {
    super::list(
        repo,
        kind,
        state,
        query,
        counts,
        items,
        &ListFilters::default(),
    )
}

/// The detail page without assignee and milestone choices, which most of these tests do not
/// render.
#[expect(
    clippy::too_many_arguments,
    reason = "The shim keeps the detail renderer's arguments as the tests already pass them."
)]
fn detail(
    repo: &Repository,
    kind: Kind,
    item: &Item,
    comments: &CommentPage,
    available_labels: &[Label],
    draft: &DetailDraft,
    error: Option<&str>,
    references: &References,
    reviewers: &[Reviewer],
) -> Element {
    super::detail(
        repo,
        kind,
        item,
        comments,
        available_labels,
        draft,
        error,
        references,
        reviewers,
        &[],
        &[],
        &[],
    )
}
fn repository() -> Repository {
    Repository {
        owner: "alice".into(),
        name: "repo".into(),
        signed_in: true,
        can_write: true,
        can_admin: true,
        csrf: "csrf-sentinel".into(),
        viewer: String::new(),
        chrome: RepoChrome {
            owner: "alice".into(),
            name: "repo".into(),
            signed_in: true,
            can_write: true,
            can_admin: true,
            is_owner: true,
            stars: 7,
            watchers: 3,
            forks: 2,
            csrf_token: "csrf-sentinel".into(),
            current_path: "/alice/repo/issues".into(),
            ..Default::default()
        },
    }
}
fn repository_at(kind: Kind, tail: &str) -> Repository {
    let mut repo = repository();
    repo.chrome.current_path = format!("{}{tail}", repo.family_url(kind));
    repo
}
fn item() -> Item {
    Item {
        id: 7,
        number: 7,
        stacked: false,
        stack: None,
        stack_candidate: None,
        assignees: Vec::new(),
        milestone: None,
        title: "<script>alert(1)</script>".into(),
        body: "safe".into(),
        state: "open".into(),
        author: "alice".into(),
        created: "today".into(),
        updated: "revision-1".into(),
        labels: vec![],
        comment_count: 0,
        can_tick: false,
        tasks: Default::default(),
        commit_count: None,
        file_count: None,
        check_count: 0,
        review_state: None,
        check_state: None,
        reactions: Vec::new(),
        comparison: Some(Comparison {
            head: "topic".into(),
            head_label: "topic".into(),
            head_url: Some(file_url(&repository().url(), "tree", "topic", "")),
            can_run_workflows: false,
            base: "main".into(),
            head_sha: "abc".into(),
            base_sha: "def".into(),
            draft: false,
            can_ready: false,
            merge_commit_only: false,
            merge_commit_refused: false,
            merged_commit: None,
            merge_target: "main".into(),
            merge_with: vec![],
            blockers: vec![],
            can_vote: true,
            merge_methods: vec!["merge".into(), "squash".into()],
            has_checks: false,
            failed_checks: 0,
            checks_running: false,
            has_workflows: false,
        }),
    }
}
fn page_modes() -> Vec<Page> {
    let mut modes = Vec::new();
    let diff = Diff {
        files: vec![DiffFile {
            filename: "<img src=x onerror=evil>".into(),
            status: "added".into(),
            additions: 1,
            deletions: 0,
            patch: "<script>patch()</script>".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 0,
        viewed: None,
    };
    for kind in [Kind::Issue, Kind::Pull] {
        let mut entry = item();
        if kind == Kind::Issue {
            entry.comparison = None;
        }
        modes.push(Page::List {
            repository: repository_at(kind, ""),
            kind,
            state: "all".into(),
            query: String::new(),
            counts: ListCounts::default(),
            items: vec![entry.clone()],
            filters: ListFilters::default(),
        });
        if kind == Kind::Issue {
            modes.push(Page::Choose {
                repository: repository_at(Kind::Issue, "/new"),
                templates: vec![TemplateCard {
                    path: ".github/ISSUE_TEMPLATE/<bug>.md".into(),
                    name: "<Bug> report".into(),
                    about: "</a><script>choose()</script>".into(),
                }],
                blank_allowed: true,
            });
        }
        for preview in [false, true] {
            modes.push(Page::New {
                repository: repository_at(kind, "/new"),
                kind,
                draft: NewDraft {
                    head_repo: String::new(),
                    title: "  submitted <title>  ".into(),
                    body: "</textarea><script>draft()</script>".into(),
                    head: "topic/a".into(),
                    base: "main".into(),
                    draft: true,
                    ..Default::default()
                },
                branches: BranchChoices {
                    base: vec!["main".into(), "topic/a".into()],
                    head: vec!["main".into(), "topic/a".into()],
                    ..Default::default()
                },
                choices: Default::default(),
                diff: if kind == Kind::Pull {
                    Some(diff.clone())
                } else {
                    None
                },
                error: Some("Submission changed; preserved <text>".into()),
                preview,
                references: Default::default(),
            });
        }
        modes.push(Page::Detail {
            repository: repository_at(kind, "/7"),
            kind,
            item: entry,
            comments: CommentPage {
                events_next: None,
                comments: vec![Comment {
                    revision: "comment-revision".into(),
                    id: 23,
                    author: "alice".into(),
                    body: "<script>comment()</script>".into(),
                    created: "today".into(),
                    location: Some("src/<file>:12 (RIGHT)".into()),
                    conversation: None,
                    reactions: Vec::new(),
                    can_edit: false,
                    can_delete: false,
                    edited: None,
                    edit: None,
                }],
                reviews: Vec::new(),
                page: 2,
                has_more: true,
                events: Vec::new(),
            },
            available_labels: vec![Label {
                id: 11,
                name: "<bug>".into(),
                color: "bad; background:red".into(),
                description: String::new(),
            }],
            draft: DetailDraft {
                title: Some("preserved & title".into()),
                body: Some("</textarea><script>edit()</script>".into()),
                comment: "preserved comment".into(),
                repository_id: None,
                issue_id: None,
                editing: None,
                expected_body: String::new(),
                label: "11".into(),
                preview: true,
                merge_method: String::new(),
                reviewer: String::new(),
            },
            error: Some("Stale revision; review your preserved submission.".into()),
            references: Default::default(),
            reviewers: Vec::new(),
            reviewer_choices: Vec::new(),
            assignable: Vec::new(),
            milestones: Vec::new(),
        });
    }
    let mut compare_repo = repository();
    compare_repo.chrome.current_path = "/alice/repo/compare/main...topic/a".into();
    for compared in [Some(diff.clone()), None] {
        modes.push(Page::Compare {
            repository: compare_repo.clone(),
            base: "main".into(),
            head: "<script>topic</script>".into(),
            branches: vec!["main".into(), "<script>topic</script>".into()],
            diff: compared,
            open_pull: None,
            split: false,
        });
    }
    modes.push(Page::Diff {
        repository: repository_at(Kind::Pull, "/7/diff"),
        item: item(),
        diff,
        threads: Vec::new(),
        truncated: false,
        composer: None,
        review: None,
        pending_comments: 0,
        error: None,
        references: Default::default(),
        split: false,
    });
    let mut commit_tab = item();
    commit_tab.commit_count = Some(1);
    modes.push(Page::PullCommits {
        repository: repository_at(Kind::Pull, "/7/commits"),
        item: commit_tab,
        commits: vec![Commit {
            sha: "a".repeat(40),
            message: "<script>alert(1)</script>".into(),
            author: "alice".into(),
            author_date: "2026-10-01T10:00:00+00:00".into(),
            committer: "alice".into(),
            committer_date: "2026-10-01T10:00:00+00:00".into(),
            parents: Vec::new(),
        }],
    });
    modes.push(Page::PullChecks {
        repository: repository_at(Kind::Pull, "/7/checks"),
        item: item(),
        runs: vec![run_fixture("<script>run</script>", "success")],
        statuses: Vec::new(),
    });
    let hostile = "<script>alert(1)</script>";
    modes.push(Page::Milestones {
        repository: repository(),
        state: "open".into(),
        counts: ListCounts::default(),
        items: vec![milestone(3, hostile, 1, 3)],
        sort: String::new(),
        page_number: 1,
        has_next: false,
    });
    modes.push(Page::Labels {
        repository: repository(),
        items: vec![Label {
            id: 9,
            name: hostile.into(),
            color: "#0075ca".into(),
            description: hostile.into(),
        }],
        usage: Default::default(),
        query: String::new(),
        sort: String::new(),
        page_number: 1,
        has_next: false,
    });
    modes.push(Page::MilestoneNew {
        repository: repository(),
        draft: MilestoneDraft {
            title: hostile.into(),
            ..Default::default()
        },
        error: Some("Milestone title must contain 1–255 bytes of text.".into()),
    });
    modes.push(Page::MilestoneDetail {
        repository: repository(),
        milestone: milestone(3, hostile, 1, 3),
        state: "all".into(),
        items: vec![item()],
        pull_requests: vec![item()],
        pagination: (1, false),
        draft: MilestoneDraft {
            repository_id: None,
            title: hostile.into(),
            description: "Ship it".into(),
            due_date: "2026-10-20".into(),
        },
        error: None,
    });
    modes
}
#[test]
fn safe_page_json_roundtrips_every_variant_and_recovery_mode() {
    let modes = page_modes();
    assert_eq!(modes.len(), 18);
    for page in modes {
        let encoded = serde_json::to_string(&page).unwrap();
        let decoded: Page = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, page);
    }
}
#[test]
fn typed_dispatch_matches_pure_views_after_json_roundtrip_and_escapes_text() {
    crate::webui::tests::in_scope(|| {
        for page in page_modes() {
            let decoded: Page =
                serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
            let plain = match &page {
                Page::Choose {
                    repository,
                    templates,
                    blank_allowed,
                } => new::chooser(repository, templates, *blank_allowed),
                Page::Milestones {
                    repository,
                    state,
                    counts,
                    items,
                    sort,
                    page_number,
                    has_next,
                } => milestones::list(
                    repository,
                    state,
                    *counts,
                    items,
                    sort,
                    *page_number,
                    *has_next,
                ),
                Page::Labels {
                    repository,
                    items,
                    usage,
                    query,
                    sort,
                    page_number,
                    has_next,
                } => labels::list(
                    repository,
                    items,
                    query,
                    sort,
                    usage,
                    *page_number,
                    *has_next,
                ),
                Page::MilestoneNew {
                    repository,
                    draft,
                    error,
                } => milestones::new(repository, draft, error.as_deref()),
                Page::MilestoneDetail {
                    repository,
                    milestone,
                    state,
                    items,
                    pull_requests,
                    pagination,
                    draft,
                    error,
                } => milestones::detail(
                    repository,
                    milestone,
                    state,
                    items,
                    pull_requests,
                    *pagination,
                    draft,
                    error.as_deref(),
                ),
                Page::List {
                    repository,
                    kind,
                    state,
                    query,
                    counts,
                    items,
                    filters,
                } => super::list(repository, *kind, state, query, *counts, items, filters),
                Page::New {
                    repository,
                    kind,
                    draft,
                    branches,
                    choices,
                    diff,
                    error,
                    preview,
                    references,
                } => new(
                    repository,
                    *kind,
                    draft,
                    branches,
                    choices,
                    diff.as_ref(),
                    error.as_deref(),
                    *preview,
                    references,
                ),
                Page::Detail {
                    repository,
                    kind,
                    item,
                    comments,
                    available_labels,
                    draft,
                    error,
                    references,
                    reviewers,
                    assignable,
                    milestones,
                    reviewer_choices,
                } => super::detail(
                    repository,
                    *kind,
                    item,
                    comments,
                    available_labels,
                    draft,
                    error.as_deref(),
                    references,
                    reviewers,
                    reviewer_choices,
                    assignable,
                    milestones,
                ),
                Page::Compare {
                    repository,
                    base,
                    head,
                    branches,
                    diff,
                    open_pull,
                    split,
                } => compare(
                    repository,
                    base,
                    head,
                    branches,
                    diff.as_ref(),
                    open_pull.as_ref(),
                    *split,
                ),
                Page::Diff {
                    repository,
                    item,
                    diff,
                    threads,
                    truncated,
                    composer,
                    review,
                    pending_comments,
                    error,
                    references,
                    split,
                } => diff_page(
                    repository,
                    item,
                    diff,
                    threads,
                    *truncated,
                    composer.as_ref(),
                    review.as_ref(),
                    *pending_comments,
                    error.as_deref(),
                    references,
                    *split,
                ),
                Page::PullCommits {
                    repository,
                    item,
                    commits,
                } => commits_page(repository, item, commits),
                Page::PullChecks {
                    repository,
                    item,
                    runs,
                    statuses,
                } => checks_page(repository, item, runs, statuses),
            };
            let html = dioxus_ssr::render_element(render(decoded));
            assert_eq!(html, dioxus_ssr::render_element(plain));
            assert!(!html.contains("<script>"));
            assert!(!html.contains("<img src=x"));
            assert!(html.contains("&lt;script&gt;") || html.contains("&#60;script&#62;"));
        }
    });
}
#[test]
fn serialized_repository_contains_only_public_presentation_and_form_fields() {
    let json = serde_json::to_value(Page::List {
        repository: repository(),
        kind: Kind::Issue,
        state: "open".into(),
        query: String::new(),
        counts: ListCounts::default(),
        items: vec![],
        filters: ListFilters::default(),
    })
    .unwrap();
    let repo = json["List"]["repository"].as_object().unwrap();
    assert_eq!(repo.len(), 8);
    for name in [
        "owner",
        "name",
        "signed_in",
        "can_write",
        "can_admin",
        "csrf",
        "viewer",
        "chrome",
    ] {
        assert!(repo.contains_key(name));
    }
    for forbidden in [
        "disk_path",
        "pool",
        "redis",
        "password",
        "password_hash",
        "email",
        "session_id",
        "access_token",
    ] {
        assert!(!repo.contains_key(forbidden));
        assert!(!repo["chrome"].as_object().unwrap().contains_key(forbidden));
    }
    assert_eq!(repo["csrf"], "csrf-sentinel");
    assert_eq!(repo["chrome"]["csrf_token"], "csrf-sentinel");
    assert_eq!(repo["chrome"]["stars"], 7);
    assert_eq!(repo["chrome"]["watchers"], 3);
    assert_eq!(repo["chrome"]["forks"], 2);
}
#[test]
fn preview_enhancement_preserves_native_action_and_initial_html_without_browser() {
    let html = dioxus_ssr::render_element(rsx! {
        form { method: "post", action: "/alice/repo/issues/7/edit",
            input { r#type: "hidden", name: "csrf_token", value: "csrf-sentinel" }
            MarkdownEditor { id: "edit-body", initial: "**immediate preview**", rows: 10, required: false, initial_preview: true }
        }
    });
    assert!(html.contains("name=\"body\""));
    assert!(html.contains("name=\"intent\""));
    assert!(html.contains("value=\"preview\""));
    assert!(html.contains("**immediate preview**"));
    assert!(html.contains("<strong"));
    assert!(html.contains(">immediate preview</strong>"));
    assert!(html.contains("csrf-sentinel"));
}
#[test]
fn empty_preview_says_there_is_nothing_to_preview() {
    let html = dioxus_ssr::render_element(rsx! {
        form { method: "post", action: "/alice/repo/issues/7/edit",
            MarkdownEditor { id: "edit-body", initial: "", rows: 10, required: false, initial_preview: true }
        }
    });
    assert!(html.contains("Nothing to preview."), "{html}");
}
#[test]
fn prerendered_markdown_draft_preserves_raw_text_and_initial_preview() {
    let source = "\n**kept preview**\n</textarea><script>bad</script>&";
    let mut renderer = dioxus_ssr::Renderer::new();
    renderer.pre_render = true;
    let html = renderer.render_element(rsx! {
        form { method: "post", action: "/alice/repo/issues/7/edit",
            input { r#type: "hidden", name: "csrf_token", value: "csrf-fixture" }
            MarkdownEditor { id: "edit-body", initial: source, rows: 10, required: false, initial_preview: true }
        }
    });
    let textarea = html.split_once("<textarea").unwrap().1;
    let text = textarea
        .split_once('>')
        .unwrap()
        .1
        .split_once("</textarea>")
        .unwrap()
        .0;
    assert_eq!(
        text,
        "\n\n**kept preview**\n&lt;/textarea&gt;&lt;script&gt;bad&lt;/script&gt;&amp;"
    );
    assert!(!text.contains("<!--"));
    assert!(!text.contains("<script>"));
    assert!(html.contains("name=\"body\""));
    assert!(html.contains("name=\"csrf_token\""));
    assert!(html.contains("value=\"preview\""));
    assert!(html.contains("<strong"));
    assert!(html.contains("kept preview"));
}

#[test]
fn urls_encode_each_segment_and_colors_cannot_inject_css() {
    assert_eq!(repo_url("a/b", "x y"), "/a%2Fb/x%20y");
    assert!(!label_style("red; background:url(javascript:evil)").contains("javascript"));
    assert!(label_style("#12aBcF").contains("#12aBcF"));
    assert_eq!(label_hex("12aBcF").as_deref(), Some("#12aBcF"));
    assert_eq!(label_hex("#12aBcF").as_deref(), Some("#12aBcF"));
    assert_eq!(label_hex("red; background:url(x)"), None);
    assert_eq!(label_hex("#12345"), None);
}

#[test]
fn label_chips_use_their_own_color_with_readable_text() {
    let light = label_style("#ededed");
    assert!(light.contains("background: #ededed"));
    assert!(light.contains("color: #1f2328"));
    let dark = label_style("#0e8a16");
    assert!(dark.contains("background: #0e8a16"));
    assert!(dark.contains("color: #ffffff"));
}
#[test]
fn editor_preserves_text_and_has_native_csrf_forms() {
    let draft = NewDraft {
        title: "submitted & title".into(),
        body: "</textarea><script>bad()</script>".into(),
        ..Default::default()
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(new(
            &repository(),
            Kind::Issue,
            &draft,
            &BranchChoices::default(),
            &IssueChoices::default(),
            None,
            Some("Invalid title"),
            false,
            &Default::default(),
        ))
    });
    assert!(html.contains("method=\"post\""));
    assert!(html.contains("name=\"csrf_token\""));
    assert!(html.contains("csrf-sentinel"));
    assert!(html.contains("submitted &#38; title"));
    assert!(html.contains("&lt;/textarea&gt;&lt;script&gt;"));
    assert!(!html.contains("<script>bad()"));
    assert!(html.contains("Invalid title"));
}
#[test]
fn discussion_pages_link_to_the_neighbouring_pages() {
    let render = |page, has_more| {
        let comments = CommentPage {
            events_next: None,
            comments: vec![Comment {
                revision: "comment-revision".into(),
                id: 1,
                author: "alice".into(),
                body: "Text".into(),
                created: "today".into(),
                location: None,
                conversation: None,
                reactions: Vec::new(),
                can_edit: false,
                can_delete: false,
                edited: None,
                edit: None,
            }],
            reviews: Vec::new(),
            page,
            has_more,
            events: Vec::new(),
        };
        crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(detail(
                &repository(),
                Kind::Issue,
                &item(),
                &comments,
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        })
    };
    let first = render(1, true);
    assert!(!first.contains("Previous"));
    assert!(first.contains("issues/7?page=2"));
    let middle = render(2, true);
    assert!(middle.contains("issues/7?page=1"));
    assert!(middle.contains("issues/7?page=3"));
    let last = render(2, false);
    assert!(last.contains("issues/7?page=1"));
    assert!(!last.contains("issues/7?page=3"));
    assert!(!render(1, false).contains("Discussion pages"));
}

#[test]
fn detail_forms_carry_expected_state_and_merge_revisions() {
    crate::webui::tests::in_scope(|| {
        let html = dioxus_ssr::render_element(detail(
            &repository(),
            Kind::Pull,
            &item(),
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        for field in [
            "csrf_token",
            "expected_state",
            "expected_updated",
            "expected_head",
            "expected_base",
            "expected_base_branch",
            "expected_target",
        ] {
            assert!(html.contains(&format!("name=\"{field}\"")));
        }
        assert!(html.contains("/alice/repo/pulls/7/merge"));
        assert!(html.contains("<details"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&#60;script&#62;alert(1)&#60;/script&#62;"));
    });
}
#[test]
fn merge_panel_lists_what_blocks_the_merge_and_escapes_it() {
    crate::webui::tests::in_scope(|| {
        let render = |entry: &Item| {
            dioxus_ssr::render_element(detail(
                &repository(),
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let html = render(&item());
        assert!(!html.contains("collaboration-blockers"));
        assert!(!html.contains("Merging is blocked"));

        let mut blocked = item();
        blocked.comparison.as_mut().unwrap().blockers = vec![
            "1 approving review required, found 0".into(),
            "<img src=x onerror=alert(1)>".into(),
        ];
        let html = render(&blocked);
        assert!(html.contains("Merging is blocked"));
        assert!(html.contains("<li>1 approving review required, found 0</li>"));
        assert!(html.contains("&#60;img src=x onerror=alert(1)&#62;"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("name=\"confirm\""));
    });
}

#[test]
fn stack_card_marks_the_current_layer_and_puts_base_on_its_own_line() {
    crate::webui::tests::in_scope(|| {
        let render = |entry: &Item, repo: &Repository| {
            dioxus_ssr::render_element(detail(
                repo,
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let mut stacked = item();
        stacked.stack = Some(StackMap {
            trunk: "main".into(),
            position: 2,
            restack: false,
            layers: vec![
                StackLayer {
                    number: 6,
                    title: "Root change".into(),
                    state: "merged".into(),
                    draft: false,
                    current: false,
                },
                StackLayer {
                    number: 7,
                    title: "Second change".into(),
                    state: "open".into(),
                    draft: true,
                    current: true,
                },
            ],
        });
        let html = render(&stacked, &repository_at(Kind::Pull, "/7"));
        let stack = html.split_once("collaboration-stack\"").unwrap().1;
        let layers = stack.split_once("</ol>").unwrap().0;
        assert!(layers.contains("collaboration-stack-layer collaboration-stack-current"));
        assert!(layers.contains("aria-current=\"page\""));
        assert!(layers.contains("Draft"));
        assert_eq!(layers.matches("collaboration-stack-current").count(), 1);
        let base = stack
            .split_once("class=\"collaboration-stack-base\"")
            .unwrap()
            .1
            .split_once("</p>")
            .unwrap()
            .0;
        assert!(base.contains("Base"));
        assert!(base.contains("main"));
        assert!(stack.find("</ol>").unwrap() < stack.find("collaboration-stack-base").unwrap());
    });
}

#[test]
fn timeline_rail_marks_only_discussion_entries() {
    crate::webui::tests::in_scope(|| {
        let comments = CommentPage {
            events_next: None,
            comments: vec![comment_at(1, "2026-10-02T12:00:00+00:00")],
            reviews: vec![review(5, "approved", "2026-10-02T13:00:00+00:00")],
            page: 1,
            has_more: false,
            events: Vec::new(),
        };
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &comments,
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        // The description, each comment and each review are the entries on the rail.
        let thread_at = html.find("class=\"collaboration-thread\"").unwrap();
        let thread_end = html.find("collaboration-merge-box").unwrap();
        let thread = &html[thread_at..thread_end];
        assert!(thread.contains("opened "));
        assert!(thread.contains("id=\"comment-1\""));
        assert!(thread.contains("id=\"review-5\""));
        // The description, the comment and the review each show their author's picture.
        assert!(thread.contains("src=\"/avatars/"));
        assert!(thread.matches("class=\"avatar\"").count() >= 3);
        for control in [
            "/edit\"",
            "/state\"",
            "/merge\"",
            "/comments\"",
            "id=\"review-form\"",
        ] {
            assert!(!thread.contains(control), "{control} is not an entry");
        }
        // Edit sits under the title and Close beside the composer, both outside the rail.
        assert!(html.find("/edit\"").unwrap() < thread_at);
        assert!(html.find("/state\"").unwrap() > thread_end);
        let css = include_str!("../../../components/native.css");
        assert!(css.contains(".collaboration-thread > *::before"));
        assert!(!css.contains(".collaboration-timeline::before"));
    });
}

#[test]
fn merge_box_disables_merge_commits_on_a_linear_branch() {
    crate::webui::tests::in_scope(|| {
        let writer = repository_at(Kind::Pull, "/7");
        let mut linear = item();
        let compare = linear.comparison.as_mut().unwrap();
        compare.merge_methods = vec!["merge".into(), "squash".into(), "rebase".into()];
        compare.merge_commit_refused = true;
        let html = dioxus_ssr::render_element(detail(
            &writer,
            Kind::Pull,
            &linear,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        // The confirm checkbox also has value "merge", so find the merge method radio by its name.
        assert!(tag_with(&html, "name=\"merge_method\" value=\"merge\"").contains("disabled"));
        assert!(!tag_with(&html, "value=\"squash\"").contains("disabled"));
        assert!(tag_with(&html, "value=\"squash\"").contains("checked"));
        assert!(html.contains(MERGE_COMMIT_REFUSED));
    });
}

#[test]
fn merge_box_shows_blockers_to_writers_and_a_status_to_everyone_else() {
    crate::webui::tests::in_scope(|| {
        let render = |entry: &Item, repo: &Repository| {
            dioxus_ssr::render_element(detail(
                repo,
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let writer = repository_at(Kind::Pull, "/7");
        let mut blocked = item();
        blocked.comparison.as_mut().unwrap().blockers =
            vec!["1 approving review required, found 0".into()];
        let html = render(&blocked, &writer);
        let box_at = html.find("collaboration-merge-box").unwrap();
        assert!(html.contains("Merging is blocked"));
        assert!(!html.contains("Ready to merge"));
        // The box sits after the discussion and before the comment form.
        assert!(html.find("class=\"collaboration-thread\"").unwrap() < box_at);
        assert!(box_at < html.find("collaboration-comment-form").unwrap());
        let blockers_at = html.find("collaboration-blockers").unwrap();
        assert!(box_at < blockers_at);
        assert!(blockers_at < html.find("/alice/repo/pulls/7/merge\"").unwrap());
        assert!(html.contains("name=\"confirm\""));

        // Nothing has checked the head commit and nobody was asked to review, so no green check.
        let unchecked = render(&item(), &writer);
        assert!(unchecked.contains("No checks configured"));
        assert!(!unchecked.contains("Ready to merge"));
        assert!(!unchecked.contains("Merging is blocked"));
        // The link to the Actions page appears only when the head commit has workflows to run.
        assert!(!unchecked.contains("href=\"/alice/repo/actions\""));
        let mut with_workflows = item();
        with_workflows.comparison.as_mut().unwrap().has_workflows = true;
        let linked = render(&with_workflows, &writer);
        assert!(linked.contains("href=\"/alice/repo/actions\""));
        assert!(linked.contains("No checks configured"));

        let mut checked = item();
        checked.comparison.as_mut().unwrap().has_checks = true;
        let ready = render(&checked, &writer);
        assert!(ready.contains("All checks have passed"));
        assert!(!ready.contains("Ready to merge"));
        assert!(!ready.contains("Some checks were not successful"));
        assert!(!ready.contains("Merging is blocked"));
        assert!(!ready.contains("collaboration-blockers"));

        // A failed check is not a green check: the count and a link to the Checks tab say why.
        let mut failing = item();
        let failing_compare = failing.comparison.as_mut().unwrap();
        failing_compare.has_checks = true;
        failing_compare.failed_checks = 2;
        let failed = render(&failing, &writer);
        assert!(failed.contains("Some checks were not successful"));
        assert!(failed.contains("2 checks failed."));
        assert!(failed.contains("href=\"/alice/repo/pulls/7/checks\""));
        assert!(!failed.contains("All checks have passed"));
        assert!(!failed.contains("Ready to merge"));
        assert!(!failed.contains("Merging is blocked"));
        // The merge stays on offer; the failure only changes the mark and the words.
        assert!(failed.contains("Merge pull request"));
        assert!(failed.contains("name=\"merge_method\""));

        let mut one_failing = item();
        one_failing.comparison.as_mut().unwrap().failed_checks = 1;
        one_failing.comparison.as_mut().unwrap().has_checks = true;
        assert!(render(&one_failing, &writer).contains("1 check failed."));

        let mut running = item();
        let running_compare = running.comparison.as_mut().unwrap();
        running_compare.has_checks = true;
        running_compare.checks_running = true;
        let pending = render(&running, &writer);
        assert!(pending.contains("Checks are still running"));
        assert!(!pending.contains("All checks have passed"));
        assert!(!pending.contains("Ready to merge"));
        assert!(ready.contains("name=\"merge_method\""));

        let mut reader = repository_at(Kind::Pull, "/7");
        reader.can_write = false;
        reader.can_admin = false;
        reader.chrome.can_write = false;
        reader.chrome.can_admin = false;
        let read = render(&blocked, &reader);
        assert!(read.contains("Only people with write access can merge it."));
        assert!(!read.contains("/merge\""));
        assert!(!read.contains("collaboration-blockers"));

        let mut merged = item();
        merged.state = "merged".into();
        let html = render(&merged, &writer);
        assert!(html.contains("This pull request was merged."));
        merged.comparison.as_mut().unwrap().merged_commit = Some("0123456789abcdef".into());
        let html = render(&merged, &writer);
        assert!(html.contains("This pull request was merged into main as"));
        assert!(html.contains("class=\"collaboration-merge-sha\""), "{html}");
        assert!(html.contains("/alice/repo/commit/0123456789abcdef"));
        assert!(html.contains(">0123456<"));
        assert!(!html.contains("/merge\""));
        assert!(!html.contains("collaboration-merge-box"));
    });
}

#[test]
fn merge_box_names_a_change_request_beside_what_branch_protection_allows() {
    crate::webui::tests::in_scope(|| {
        let requested = [Reviewer {
            username: "bob".into(),
            status: ReviewerStatus::ChangesRequested,
            stale: false,
            pending: false,
            code_owner: false,
        }];
        let render = |entry: &Item| {
            dioxus_ssr::render_element(super::detail(
                &repository_at(Kind::Pull, "/7"),
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &requested,
                &[],
                &[],
                &[],
            ))
        };
        // Branch protection allows this merge, so the box says so beside the open request.
        let html = render(&item());
        assert!(html.contains("Changes requested"), "{html}");
        assert!(
            html.contains("Changes requested by bob. Branch protection still allows this merge.")
        );
        assert!(!html.contains("Ready to merge"));
        // A blocker makes the same request read as a block.
        let mut blocked = item();
        blocked.comparison.as_mut().unwrap().blockers =
            vec!["1 approving review required, found 0".into()];
        let html = render(&blocked);
        assert!(html.contains("Merging is blocked"));
        assert!(html.contains("Changes requested by bob. Branch protection blocks this merge until the blockers below are resolved."));
    });
}

#[test]
fn merge_button_is_disabled_only_while_the_merge_is_blocked() {
    crate::webui::tests::in_scope(|| {
        let render = |entry: &Item| {
            dioxus_ssr::render_element(detail(
                &repository(),
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let ready = render(&item());
        assert!(!tag_with(&ready, "Merge pull request").contains("disabled"));

        let mut blocked = item();
        blocked.comparison.as_mut().unwrap().blockers =
            vec!["1 approving review required, found 0".into()];
        let html = render(&blocked);
        assert!(tag_with(&html, "Merge pull request").contains("disabled"));
        assert!(html.contains("Merging is blocked"));
        assert!(html.contains("name=\"confirm\""));
    });
}

/// The changed-file list of the test pull request: one modified file.
fn files_diff() -> Diff {
    Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    }
}

/// The Files changed page with the viewer's review draft and pending comment count.
fn files_html(
    repo: &Repository,
    entry: &Item,
    review: Option<&ReviewDraft>,
    pending: usize,
) -> String {
    dioxus_ssr::render_element(diff_page(
        repo,
        entry,
        &files_diff(),
        &[],
        false,
        None,
        review,
        pending,
        None,
        &Default::default(),
        false,
    ))
}

#[test]
fn review_changes_menu_offers_each_decision_as_a_described_option_row() {
    let mut voter = repository_at(Kind::Pull, "/7/diff");
    voter.viewer = "bob".into();
    let html = files_html(&voter, &item(), None, 0);
    let form_at = html.find("id=\"review-form\"").unwrap();
    let form = &html[form_at..];
    let form = &form[..form.find("</form>").unwrap()];
    assert!(form.contains("Review</legend>"));
    for (value, title, note) in [
        (
            "commented",
            "Comment",
            "Share feedback without approving or blocking.",
        ),
        ("approved", "Approve", "Approve these changes for merging."),
        (
            "changes_requested",
            "Request changes",
            "Ask the author to make changes before merging.",
        ),
    ] {
        assert!(form.contains(&format!("value=\"{value}\"")), "{value}");
        assert!(
            form.contains(&format!("<strong>{title}</strong>")),
            "{title}"
        );
        assert!(form.contains(note), "{note}");
    }
    assert_eq!(form.matches("class=\"collaboration-option\"").count(), 3);
    assert_eq!(form.matches("type=\"radio\"").count(), 3);
    assert!(!tag_with(&html, "value=\"approved\"").contains("disabled"));
    assert!(form.contains("Submit review</button>"));

    // Someone without write access may comment, and the two decisions say why they are unavailable.
    let mut reader = repository_at(Kind::Pull, "/7/diff");
    reader.viewer = "dave".into();
    reader.can_write = false;
    let mut reader_item = item();
    reader_item.comparison.as_mut().unwrap().can_vote = false;
    let html = files_html(&reader, &reader_item, None, 0);
    assert!(
        html.contains("Approving and requesting changes needs write access to this repository.")
    );
    assert!(tag_with(&html, "value=\"approved\"").contains("disabled"));
    assert!(tag_with(&html, "value=\"changes_requested\"").contains("disabled"));
    assert!(!tag_with(&html, "value=\"commented\"").contains("disabled"));

    // The author of a pull request may comment on it, but not vote on it.
    let mut author = repository_at(Kind::Pull, "/7/diff");
    author.viewer = "alice".into();
    let mut own = item();
    own.comparison.as_mut().unwrap().can_vote = false;
    let html = files_html(&author, &own, None, 0);
    assert!(html.contains(
        "You opened this pull request, so you can comment but not approve or request changes."
    ));
    assert!(tag_with(&html, "value=\"approved\"").contains("disabled"));
    assert!(tag_with(&html, "value=\"changes_requested\"").contains("disabled"));
}
#[test]
fn review_changes_menu_has_its_own_heading_and_summary_label() {
    let mut repo = repository_at(Kind::Pull, "/7/diff");
    repo.viewer = "bob".into();
    let html = files_html(&repo, &item(), None, 0);
    let form_at = html.find("id=\"review-form\"").unwrap();
    let form = &html[form_at..];
    let form = &form[..form.find("</form>").unwrap()];
    assert!(form.contains("Review changes</h3>"));
    assert!(form.contains("Summary (Markdown)</label>"));
    // The fields scroll in their own body, and the footer with Submit review comes after them.
    let body = form.find("collaboration-review-body").unwrap();
    let box_at = form.find("<textarea").unwrap();
    let footer = form.find("collaboration-card-footer").unwrap();
    assert!(body < box_at && box_at < footer, "{form}");
    // The label is for screen readers; the header is the one visible title.
    assert!(form.contains("class=\"repo-sr-only\">Summary (Markdown)</label>"));
    // One review form, one summary box.
    assert_eq!(form.matches("<textarea").count(), 1);
    assert_eq!(form.matches("name=\"body\"").count(), 1);
}
#[test]
fn close_button_sits_in_the_composer_footer_and_submits_the_state_form() {
    crate::webui::tests::in_scope(|| {
        let render = |entry: &Item| {
            dioxus_ssr::render_element(detail(
                &repository_at(Kind::Pull, "/7"),
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let html = render(&item());
        let state_at = html.find("action=\"/alice/repo/pulls/7/state\"").unwrap();
        let state_form = tag_with(&html, "action=\"/alice/repo/pulls/7/state\"");
        assert!(state_form.contains("id=\"item-state-form\""));
        // An open pull request keeps Close in its merge box above the composer, and the composer
        // keeps only Comment. The merge box names the state form by id, so the forms never nest.
        let composer_at = html
            .find("action=\"/alice/repo/pulls/7/comments\"")
            .unwrap();
        let composer = &html[composer_at..state_at];
        assert!(composer.contains("</form>"));
        assert!(!composer.contains(">Close pull request</button>"));
        assert!(composer.contains("collaboration-submit-comment\">Comment</button>"));
        let merge_box = &html[html.find("collaboration-merge-box").unwrap()..composer_at];
        assert!(merge_box.contains("form=\"item-state-form\""));
        // Close is the quiet action, beside the primary Merge button.
        let close_at = merge_box.find(">Close pull request</button>").unwrap();
        let close_tag = &merge_box[merge_box[..close_at].rfind("<button").unwrap()..close_at];
        assert!(close_tag.contains("btn btn-secondary"), "{close_tag}");

        let mut closed = item();
        closed.state = "closed".into();
        let html = render(&closed);
        let composer_at = html
            .find("action=\"/alice/repo/pulls/7/comments\"")
            .unwrap();
        let composer =
            &html[composer_at..html.find("action=\"/alice/repo/pulls/7/state\"").unwrap()];
        assert!(composer.contains(">Reopen pull request</button>"));
        assert!(!composer.contains(">Close pull request</button>"));
    });
}

#[test]
fn unauthorized_view_has_no_management_forms() {
    let mut repo = repository();
    repo.signed_in = false;
    repo.can_write = false;
    repo.can_admin = false;
    repo.chrome.signed_in = false;
    repo.chrome.can_write = false;
    repo.chrome.can_admin = false;
    let html = dioxus_ssr::render_element(detail(
        &repo,
        Kind::Pull,
        &item(),
        &CommentPage::default(),
        &[],
        &DetailDraft::default(),
        None,
        &Default::default(),
        &[],
    ));
    assert!(!html.contains("method=\"post\""));
    assert!(html.contains("Sign in"));
}
#[test]
fn every_issue_mutation_form_has_csrf_and_labels_are_scoped_links() {
    let mut issue = item();
    issue.comparison = None;
    issue.labels = vec![Label {
        id: 11,
        name: "<bug>".into(),
        color: "#ff0000".into(),
        description: String::new(),
    }];
    // The picker lists every label of the repository. A label the issue lacks gives the apply form.
    let mut available = issue.labels.clone();
    available.push(Label {
        id: 12,
        name: "docs".into(),
        color: "#00ff00".into(),
        description: String::new(),
    });
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(detail(
            &repository(),
            Kind::Issue,
            &issue,
            &CommentPage::default(),
            &available,
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ))
    });
    let mut count = 0;
    for form in html.split("<form").skip(1) {
        let form = form.split("</form>").next().unwrap();
        if form.contains("method=\"post\"") {
            if form.contains("action=\"/alice/repo/issues/") {
                count += 1;
            }
            assert!(form.contains("name=\"csrf_token\""));
            assert!(form.contains("csrf-sentinel"));
        }
    }
    assert_eq!(count, 6); // edit, state, remove label, apply label, comment, react
    assert!(html.contains("/alice/repo/issues/7/labels/11/remove"));
    assert!(html.contains("&#60;bug&#62;"));
}
#[test]
fn comparison_submits_the_editor_without_creating_or_needing_a_title() {
    let draft = NewDraft {
        body: "preserved text".into(),
        head: "topic/a".into(),
        base: "main".into(),
        ..Default::default()
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(new(
            &repository(),
            Kind::Pull,
            &draft,
            &BranchChoices {
                base: vec!["main".into(), "topic/a".into()],
                head: vec!["main".into(), "topic/a".into()],
                ..Default::default()
            },
            &IssueChoices::default(),
            None,
            None,
            false,
            &Default::default(),
        ))
    });
    assert!(html.contains("value=\"compare\""));
    assert!(html.contains("formnovalidate"));
    assert!(html.contains("preserved text"));
    assert!(html.contains("name=\"csrf_token\""));
    assert!(!html.contains("<script"));
    assert!(!html.contains("onclick"));
    assert!(html.contains("collaboration-new-layout"));
    // A pull request has no picker sidebar, so its form takes the whole width.
    assert!(html.contains("collaboration-new-plain"));
}
#[test]
fn compare_page_offers_a_pull_request_only_for_changes_between_different_branches() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: "@@ -1 +1,2 @@\n-old\n+new\n+more".into(),
            owners: Vec::new(),
        }],
        additions: 2,
        deletions: 1,
        viewed: None,
    };
    let render = |base: &str, head: &str, diff: Option<&Diff>| {
        crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(compare(
                &repository(),
                base,
                head,
                &["main".into(), "topic/a".into()],
                diff,
                None,
                false,
            ))
        })
    };
    let html = render("main", "topic/a", Some(&diff));
    assert!(html.contains("Comparing changes"));
    assert!(html.contains("method=\"get\""));
    assert!(html.contains("name=\"base\""));
    assert!(html.contains("name=\"head\""));
    // The two selects, the arrow and the Compare button share one row.
    let row_at = html.find("collaboration-compare-row").unwrap();
    let compare_at = html.find(">Compare</button>").unwrap();
    assert!(row_at < html.find("name=\"head\"").unwrap());
    assert!(html.find("name=\"head\"").unwrap() < compare_at);
    assert!(!html.contains("collaboration-compare-grid"));
    assert!(html.contains("1 file changed"));
    assert!(html.contains("src/lib.rs"));
    assert!(html.contains("Create pull request"));
    assert!(html.contains("/alice/repo/pulls/new?base=main"));
    assert!(html.contains("head=topic%2Fa"));
    // The same branch has nothing to compare, and so no pull request to open.
    let same = render("main", "main", None);
    // The apostrophe is HTML-escaped in the page, so the test matches the rest of the sentence.
    assert!(same.contains("anything to compare."));
    assert!(!same.contains("Create pull request"));
    // Two branches with no file changes show that, and offer no pull request.
    let empty = Diff {
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        viewed: None,
    };
    let unchanged = render("main", "topic/a", Some(&empty));
    assert!(unchanged.contains("No file changes."));
    assert!(!unchanged.contains("Create pull request"));
    // A branch name from the address is text in the form, never markup.
    let unknown = render("main", "<b>topic", None);
    assert!(!unknown.contains("<b>topic"));
    assert!(unknown.contains("(unavailable)"));
}

#[test]
fn compare_page_links_to_the_open_pull_request_for_its_pair() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: "@@ -1 +1,2 @@\n-old\n+new\n+more".into(),
            owners: Vec::new(),
        }],
        additions: 2,
        deletions: 1,
        viewed: None,
    };
    let branches: Vec<String> = vec!["main".into(), "topic/a".into()];
    let pull = PullSummary {
        number: 2,
        title: "Add the feature".into(),
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(compare(
            &repository(),
            "main",
            "topic/a",
            &branches,
            Some(&diff),
            Some(&pull),
            false,
        ))
    });
    // The open pull request is linked to by number, with its title, and no new one is offered.
    assert!(html.contains("View pull request #2"), "{html}");
    assert!(html.contains("/alice/repo/pulls/2"), "{html}");
    assert!(html.contains("Add the feature"), "{html}");
    assert!(!html.contains("Create pull request"), "{html}");
}

#[test]
fn markdown_preview_does_not_execute_html_or_unsafe_links() {
    let draft = NewDraft {
        title: "preview".into(),
        body: "<script>alert(1)</script>\n\n[click](javascript:alert(1))".into(),
        ..Default::default()
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(new(
            &repository(),
            Kind::Issue,
            &draft,
            &BranchChoices::default(),
            &IssueChoices::default(),
            None,
            None,
            true,
            &Default::default(),
        ))
    });
    assert!(!html.contains("<script>"));
    assert!(!html.contains("href=\"javascript:"));
    assert!(html.contains("Preview"));
}
#[test]
fn merged_pull_does_not_offer_reopen_or_a_second_merge() {
    let mut merged = item();
    merged.state = "merged".into();
    let html = dioxus_ssr::render_element(detail(
        &repository(),
        Kind::Pull,
        &merged,
        &CommentPage::default(),
        &[],
        &DetailDraft::default(),
        None,
        &Default::default(),
        &[],
    ));
    assert!(!html.contains("action=\"/alice/repo/pulls/7/state\""));
    assert!(!html.contains("action=\"/alice/repo/pulls/7/merge\""));
    assert!(html.contains("/alice/repo/pulls/7/comments"));
}
#[test]
fn same_location_edit_refresh_preserves_draft_and_conflict_snapshot_together() {
    use crate::webui::{
        app::{self, PageEnvelope},
        document::Chrome,
    };
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Harness {
        initial: PageEnvelope,
        control: Rc<RefCell<Option<Signal<PageEnvelope>>>>,
    }
    fn root(props: Harness) -> Element {
        let page = use_signal(|| props.initial.clone());
        *props.control.borrow_mut() = Some(page);
        app::page_tree(page())
    }
    fn edit_form(html: &str, number: i32) -> &str {
        let action = format!("action=\"/alice/repo/issues/{number}/edit\"");
        html.split("<form")
            .find(|form| form.contains(&action))
            .unwrap()
            .split("</form>")
            .next()
            .unwrap()
    }
    let mut original = item();
    original.title = "original title".into();
    original.body = "original draft body".into();
    original.state = "open".into();
    original.updated = "original-update".into();
    original.comparison = None;
    let mut initial = PageEnvelope {
        title: "Issue".into(),
        location: "/alice/repo/issues/7".into(),
        chrome: Chrome::default(),
        page: app::Page::Collaboration(Box::new(Page::Detail {
            repository: repository(),
            kind: Kind::Issue,
            item: original,
            comments: CommentPage::default(),
            available_labels: vec![],
            draft: DetailDraft::default(),
            error: None,
            references: Default::default(),
            reviewers: Vec::new(),
            reviewer_choices: Vec::new(),
            assignable: Vec::new(),
            milestones: Vec::new(),
        })),
    };
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(
        root,
        Harness {
            initial: initial.clone(),
            control: control.clone(),
        },
    );
    dom.rebuild_in_place();
    let mut page = control.borrow().unwrap();
    if let app::Page::Collaboration(page) = &mut initial.page {
        let Page::Detail {
            repository, item, ..
        } = &mut **page
        else {
            panic!("a detail page")
        };
        repository.csrf = "refreshed-csrf".into();
        repository.chrome.csrf_token = "refreshed-csrf".into();
        item.title = "new server title".into();
        item.body = "new server body".into();
        item.state = "closed".into();
        item.updated = "new-update".into();
    }
    dom.in_runtime(|| page.set(initial.clone()));
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("new server title"));
    let form = edit_form(&html, 7);
    assert!(
        form.contains("value=\"original-update\""),
        "old draft adopted a refreshed conflict revision: {form}"
    );
    assert!(form.contains("value=\"open\""));
    assert!(form.contains("value=\"refreshed-csrf\""));
    assert!(form.contains("original title"));
    assert!(form.contains("original draft body"));
    assert!(!form.contains("new server body"));
    if let app::Page::Collaboration(page) = &mut initial.page {
        let Page::Detail { item, .. } = &mut **page else {
            panic!("a detail page")
        };
        item.number = 8;
    }
    initial.location = "/alice/repo/issues/8".into();
    dom.in_runtime(|| page.set(initial));
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    let form = edit_form(&html, 8);
    assert!(form.contains("value=\"new-update\""));
    assert!(form.contains("value=\"closed\""));
    assert!(form.contains("new server title"));
    assert!(form.contains("new server body"));
    assert!(!form.contains("original-update"));
}

#[test]
fn every_collaboration_mode_reuses_the_complete_code_chrome_outside_content() {
    crate::webui::tests::in_scope(|| {
        for page in page_modes() {
            let (repo, family) = match &page {
                Page::Choose { repository, .. } => (repository, Kind::Issue.family()),
                Page::Milestones { repository, .. }
                | Page::MilestoneNew { repository, .. }
                | Page::MilestoneDetail { repository, .. }
                | Page::Labels { repository, .. } => (repository, Kind::Issue.family()),
                Page::List {
                    repository, kind, ..
                }
                | Page::New {
                    repository, kind, ..
                }
                | Page::Detail {
                    repository, kind, ..
                } => (repository, kind.family()),
                Page::Diff { repository, .. }
                | Page::Compare { repository, .. }
                | Page::PullCommits { repository, .. }
                | Page::PullChecks { repository, .. } => (repository, Kind::Pull.family()),
            };
            let expected = dioxus_ssr::render_element(repos::chrome::chrome(&repo.chrome, rsx! {}));
            let html = dioxus_ssr::render_element(render(page.clone()));
            let (header, content) = html.split_once("class=\"repo-content ").unwrap();
            assert_eq!(
                header,
                expected.split_once("class=\"repo-content ").unwrap().0
            );
            assert!(
                !content.contains("repo-engagement "),
                "the header band draws the engagement buttons, not the page body"
            );
            assert!(content.contains("collaboration-page"));
            assert!(!content.contains("max-w-"));
            // The tabs are drawn by the site header, not by this body.
            let tabs = dioxus_ssr::render_element(repos::chrome::repo_tabs(&repo.chrome));
            for (label, suffix) in [
                ("Code", ""),
                ("Issues", "/issues"),
                ("Pull requests", "/pulls"),
                ("Actions", "/actions"),
                ("Commits", "/commits"),
                ("Branches", "/branches"),
                ("Tags", "/tags"),
                ("Settings", "/settings"),
            ] {
                let link = tabs
                    .split("<a ")
                    .find(|link| {
                        link.split_once('>')
                            .unwrap()
                            .0
                            .contains(&format!("href=\"/alice/repo{suffix}\""))
                    })
                    .unwrap();
                let link = link.split_once("</a>").unwrap().0;
                assert!(link.contains(label));
                assert!(link.contains("<svg"));
                assert_eq!(
                    link.contains("repo-tab-active"),
                    suffix == format!("/{family}")
                );
            }
            assert_eq!(tabs.matches("repo-tab-active").count(), 1);
            let engagement = dioxus_ssr::render_element(repos::chrome::repo_engagement(
                &repo.chrome,
                "repo-engagement",
            ));
            for action in ["watch-level", "star"] {
                let form = engagement
                    .split("<form")
                    .find(|form| form.contains(&format!("action=\"/alice/repo/_forms/{action}\"")))
                    .unwrap()
                    .split_once("</form>")
                    .unwrap()
                    .0;
                assert!(form.contains("name=\"csrf_token\""));
                assert!(form.contains("csrf-sentinel"));
            }
            for count in [7, 3, 2] {
                assert!(
                    engagement.contains(&format!("class=\"repo-engagement-count\">{count}</span>"))
                );
            }
        }
    });
}

#[test]
fn empty_lists_have_truthful_state_copy_and_native_filters_without_limit_jargon() {
    crate::webui::tests::in_scope(|| {
        for kind in [Kind::Issue, Kind::Pull] {
            let repo = repository_at(kind, "");
            for state in ["open", "closed", "all"] {
                let html = dioxus_ssr::render_element(list(
                    &repo,
                    kind,
                    state,
                    "",
                    ListCounts::default(),
                    &[],
                ));
                let content = html.split_once("class=\"repo-content ").unwrap().1;
                assert!(content.contains(&empty_title(kind, state, "")));
                assert!(content.contains("collaboration-empty"));
                assert!(content.contains("<svg"));
                assert!(
                    !content.contains("method=\"post\""),
                    "the list has no write form"
                );
                assert_eq!(
                    content
                        .matches("class=\"collaboration-state-filters\"")
                        .count(),
                    1
                );
                // Open and Closed are the tabs. "all" has no tab to mark as current.
                if state != "all" {
                    assert!(content.contains("aria-current=\"page\""));
                }
                for filter in ["open", "closed"] {
                    assert!(content.contains(&format!(
                        "href=\"{}?state={filter}\"",
                        repo.family_url(kind)
                    )));
                }
                assert!(!content.contains("100"));
                assert!(!content.contains("backend"));
                assert!(!content.contains("cursor"));
                assert!(!content.contains(" shown"));
                // The toolbar has the primary new-item action, and an empty open list adds one more.
                let new_items = if state == "open" { 2 } else { 1 };
                assert_eq!(
                    content
                        .matches(&format!("href=\"{}/new\"", repo.family_url(kind)))
                        .count(),
                    new_items,
                    "the toolbar and an empty open list each offer a new item"
                );
            }
        }
    });
}

#[test]
fn dense_rows_show_loaded_items_and_issue_pagination_uses_has_more() {
    crate::webui::tests::in_scope(|| {
        let repo = repository();
        let mut issue = item();
        issue.comparison = None;
        issue.created = "2026-10-02T12:34:56Z".into();
        let html = dioxus_ssr::render_element(list(
            &repo,
            Kind::Issue,
            "open",
            "",
            ListCounts::default(),
            &[issue.clone()],
        ));
        let content = html.split_once("class=\"repo-content ").unwrap().1;
        assert!(content.contains("collaboration-row"));
        assert!(content.contains("collaboration-title-link"));
        assert!(content.contains("collaboration-icon-open"));
        // The age is the text and the exact date is the tooltip.
        assert!(content.contains("datetime=\"2026-10-02T12:34:56Z\""));
        assert!(content.contains("title=\"Oct 2, 2026\""));
        assert!(!content.contains("collaboration-state-open"));
        assert!(!content.contains("latest 100"));
        let capped = dioxus_ssr::render_element(super::list(
            &repo,
            Kind::Issue,
            "all",
            "",
            ListCounts::default(),
            &vec![issue.clone(); 100],
            &ListFilters {
                page: 1,
                has_more: true,
                ..Default::default()
            },
        ));
        assert!(capped.contains("aria-label=\"Issue pages\""));
        assert!(capped.contains("page=2"));
        assert!(!capped.contains("Showing the latest 100 matching issues."));
        let last = dioxus_ssr::render_element(super::list(
            &repo,
            Kind::Issue,
            "all",
            "",
            ListCounts::default(),
            &vec![issue.clone(); 5],
            &ListFilters {
                page: 2,
                has_more: false,
                ..Default::default()
            },
        ));
        assert!(last.contains("page=1"));
        assert!(!last.contains("page=3"));
        let capped_pulls = dioxus_ssr::render_element(super::list(
            &repo,
            Kind::Pull,
            "all",
            "",
            ListCounts::default(),
            &vec![issue; 100],
            &ListFilters {
                page: 1,
                has_more: true,
                ..Default::default()
            },
        ));
        assert!(capped_pulls.contains("aria-label=\"Pull request pages\""));
        assert!(capped_pulls.contains("page=2"));
        assert!(!capped_pulls.contains("Showing the latest 100 matching pull requests."));
    });
}

#[test]
fn light_ssr_uses_shared_theme_tokens_for_every_collaboration_surface() {
    crate::webui::tests::in_scope(|| {
        let css = include_str!("../style.css");
        let collaboration_css = css
            .split_once(".collaboration-page {")
            .unwrap()
            .1
            .split_once("/* Token colors")
            .unwrap()
            .0;
        for token in [
            "var(--background)",
            "var(--foreground)",
            "var(--muted)",
            "var(--muted-foreground)",
            "var(--border)",
        ] {
            assert!(collaboration_css.contains(token));
        }
        assert!(collaboration_css.contains(".light .collaboration-page"));
        for page in page_modes() {
            let html = dioxus_ssr::render_element(rsx! { div { class: "light", {render(page)} } });
            assert!(html.contains("class=\"light\""));
            let content = html.split_once("class=\"repo-content ").unwrap().1;
            for dark_literal in [
                "#0d1117", "#161b22", "#30363d", "#c9d1d9", "#8b949e", "#58a6ff",
            ] {
                assert!(
                    !content.contains(dark_literal),
                    "hard-coded neutral dark color in collaboration body: {dark_literal}"
                );
            }
            assert!(content.contains("collaboration-page"));
        }
    });
}

#[test]
fn recorded_comparison_branches_link_to_real_encoded_tree_routes() {
    crate::webui::tests::in_scope(|| {
        let mut pull = item();
        let comparison = pull.comparison.as_mut().unwrap();
        comparison.head = "topic/a & b".into();
        comparison.head_label = comparison.head.clone();
        comparison.head_url = Some(file_url(&repository().url(), "tree", &comparison.head, ""));
        comparison.base = "release/v1".into();
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &pull,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        assert!(html.contains("href=\"/alice/repo/tree/topic/a%20%26%20b\""));
        assert!(html.contains("href=\"/alice/repo/tree/release/v1\""));
        assert!(!html.contains("Recorded revisions"));
        assert!(html.contains("<h3>Revisions</h3>"));
        assert!(html.contains("aria-label=\"Pull request details\""));
        assert!(html.contains("collaboration-metadata"));
        assert!(html.contains(">Open</dd>"));
        assert!(html.contains("No checks configured"));
        assert!(!html.contains("The backend"));
    });
}

#[test]
fn diff_is_escaped_native_html() {
    let diff = Diff {
        additions: 1,
        deletions: 0,
        files: vec![DiffFile {
            filename: "<img src=x onerror=evil>".into(),
            status: "added".into(),
            additions: 1,
            deletions: 0,
            patch: "<script>evil()</script>".into(),
            owners: Vec::new(),
        }],
        viewed: None,
    };
    let html = dioxus_ssr::render_element(diff_page(
        &repository(),
        &item(),
        &diff,
        &[],
        false,
        None,
        None,
        0,
        None,
        &Default::default(),
        false,
    ));
    assert!(html.contains("&#60;script&#62;evil()&#60;/script&#62;"));
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("<details"));
}

/// The start tag of the element that contains `needle`.
fn tag_with<'a>(html: &'a str, needle: &str) -> &'a str {
    let at = html
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in the page"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

fn review(id: i64, state: &str, created: &str) -> Review {
    Review {
        id,
        author: "bob".into(),
        state: state.into(),
        body: Some("Please check this one".into()),
        commit: "abc".into(),
        created: created.into(),
        stale: false,
        dismissed_reason: None,
        comments: Vec::new(),
        comments_more: None,
        comments_start: None,
        reactions: Vec::new(),
    }
}

#[test]
fn reaction_bar_posts_to_its_action_and_names_the_comment_or_review_it_belongs_to() {
    use super::reactions::reaction_bar;
    let reactions = [Reaction {
        content: "heart".into(),
        count: 2,
        mine: true,
    }];
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(rsx! {
            {reaction_bar(&repository(), "/alice/repo/pulls/7/react", Some(("review", 9)), &reactions)}
        })
    });
    assert!(
        html.contains("action=\"/alice/repo/pulls/7/react\""),
        "{html}"
    );
    assert!(html.contains("name=\"review\" value=\"9\""), "{html}");
    assert!(html.contains("name=\"content\" value=\"heart\""), "{html}");
    assert!(html.contains("aria-pressed=\"true\""), "{html}");
    assert!(html.contains("collaboration-reaction-count\">2<"), "{html}");
}

#[test]
fn reaction_bar_shows_counts_to_visitors_and_nothing_without_reactions() {
    use super::reactions::reaction_bar;
    let mut visitor = repository();
    visitor.signed_in = false;
    let reactions = [Reaction {
        content: "eyes".into(),
        count: 3,
        mine: false,
    }];
    let (shown, empty) = crate::webui::tests::in_scope(|| {
        (
            dioxus_ssr::render_element(rsx! {
                {reaction_bar(&visitor, "/alice/repo/issues/7/react", None, &reactions)}
            }),
            dioxus_ssr::render_element(rsx! {
                {reaction_bar(&visitor, "/alice/repo/issues/7/react", None, &[])}
            }),
        )
    });
    assert!(
        shown.contains("collaboration-reaction-count\">3<"),
        "{shown}"
    );
    assert!(!shown.contains("type=\"submit\""), "{shown}");
    assert!(empty.is_empty(), "{empty}");
}

#[test]
fn a_review_shows_its_reaction_bar_under_its_summary() {
    use crate::webui::pages::repos::pulls::review::review_entry;
    let mut summary = review(9, "commented", "2026-10-02T12:00:00+00:00");
    summary.reactions = vec![Reaction {
        content: "rocket".into(),
        count: 1,
        mine: false,
    }];
    let references = References::default();
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(rsx! {
            {review_entry(&repository(), "/alice/repo/pulls/7", 1, "bob", &summary, &references)}
        })
    });
    assert!(
        html.contains("action=\"/alice/repo/pulls/7/react\""),
        "{html}"
    );
    assert!(html.contains("name=\"review\" value=\"9\""), "{html}");
    assert!(html.contains("collaboration-reaction-count\">1<"), "{html}");
}

#[test]
fn diff_threads_show_reaction_bars_that_return_to_the_diff_except_while_pending() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let mut root = comment_at(5, "2026-10-02T12:00:00+00:00");
    root.reactions = vec![Reaction {
        content: "heart".into(),
        count: 2,
        mine: true,
    }];
    let mut reply = comment_at(6, "2026-10-02T12:10:00+00:00");
    reply.reactions = vec![Reaction {
        content: "rocket".into(),
        count: 1,
        mine: false,
    }];
    let public = Thread {
        root,
        path: "src/lib.rs".into(),
        line: Some(2),
        side: "RIGHT".into(),
        replies: vec![reply],
        pending: false,
    };
    let pending = Thread {
        root: comment_at(7, "2026-10-02T12:20:00+00:00"),
        path: "src/lib.rs".into(),
        line: Some(2),
        side: "RIGHT".into(),
        replies: Vec::new(),
        pending: true,
    };
    let html = dioxus_ssr::render_element(diff_page(
        &repository_at(Kind::Pull, "/7/diff"),
        &item(),
        &diff,
        &[public, pending],
        true,
        None,
        None,
        0,
        None,
        &Default::default(),
        false,
    ));
    assert!(html.contains("name=\"comment\" value=\"5\""), "{html}");
    assert!(html.contains("name=\"comment\" value=\"6\""), "{html}");
    assert!(html.contains("react?from=diff"), "{html}");
    assert!(html.contains("collaboration-reaction-count\">2<"), "{html}");
    // A pending thread keeps its badge, and nobody can react to it, so it has no bar.
    assert!(html.contains("id=\"thread-7\""), "{html}");
    assert!(!html.contains("name=\"comment\" value=\"7\""), "{html}");
}

#[test]
fn comment_controls_show_only_the_rights_the_viewer_has() {
    let mut mine = comment_at(31, "today");
    mine.can_edit = true;
    mine.can_delete = true;
    mine.edited = Some("2026-10-17T10:00:00Z".into());
    let mut moderated = comment_at(32, "today");
    moderated.author = "bob".into();
    moderated.can_delete = true;
    let other = comment_at(33, "today");
    let comments = CommentPage {
        events_next: None,
        comments: vec![mine, moderated, other],
        reviews: Vec::new(),
        page: 1,
        has_more: false,
        events: Vec::new(),
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(detail(
            &repository_at(Kind::Issue, "/7"),
            Kind::Issue,
            &item(),
            &comments,
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ))
    });
    assert!(
        html.contains("action=\"/alice/repo/issues/7/comments/31/edit?page=1\""),
        "{html}"
    );
    assert!(
        html.contains("action=\"/alice/repo/issues/7/comments/31/delete\""),
        "{html}"
    );
    assert!(
        html.contains("action=\"/alice/repo/issues/7/comments/32/delete\""),
        "{html}"
    );
    assert!(
        html.contains("name=\"comment_revision\" value=\"comment-revision\""),
        "{html}"
    );
    assert!(!html.contains("comments/32/edit"), "{html}");
    assert!(!html.contains("comments/33/delete"), "{html}");
    assert!(!html.contains("comments/33/edit"), "{html}");
    assert!(html.contains("Delete this comment?"), "{html}");
    assert_eq!(html.matches("collaboration-edited\"").count(), 1, "{html}");
    assert!(
        html.contains("name=\"csrf_token\" value=\"csrf-sentinel\""),
        "{html}"
    );
}

fn comment_at(id: i64, created: &str) -> Comment {
    Comment {
        revision: "comment-revision".into(),
        id,
        author: "alice".into(),
        body: "Text".into(),
        created: created.into(),
        location: None,
        conversation: None,
        reactions: Vec::new(),
        can_edit: false,
        can_delete: false,
        edited: None,
        edit: None,
    }
}

#[test]
fn review_changes_menu_keeps_a_refused_review_and_counts_pending_comments() {
    let mut repo = repository_at(Kind::Pull, "/7/diff");
    repo.viewer = "bob".into();
    let refused = ReviewDraft {
        state: "changes_requested".into(),
        body: "Needs tests".into(),
        preview: false,
        commit: String::new(),
    };
    let html = files_html(&repo, &item(), Some(&refused), 2);
    assert!(html.contains("action=\"/alice/repo/pulls/7/reviews\""));
    assert!(tag_with(&html, "native-menu collaboration-review-menu").contains(" open"));
    assert!(tag_with(&html, "name=\"commit_sha\"").contains("value=\"abc\""));
    assert!(tag_with(&html, "value=\"changes_requested\"").contains("checked"));
    assert!(!tag_with(&html, "value=\"commented\"").contains("checked"));
    assert!(html.contains("Needs tests"));
    // The menu's count and the discard form show the pending comments of this viewer.
    assert!(html.contains("Comments waiting in your pending review"));
    assert!(html.contains(">2</span>"));
    assert!(html.contains("action=\"/alice/repo/pulls/7/reviews/pending/discard\""));
    // Discard review asks first, in the same centred dialog as the other destructive confirmations.
    assert!(html.contains("class=\"repo-confirm-modal\""));
    assert!(html.contains("This deletes 2 pending comments."));

    // With nothing pending there is no count and nothing to discard.
    let html = files_html(&repo, &item(), None, 0);
    assert!(!html.contains("Comments waiting in your pending review"));
    assert!(!html.contains("/reviews/pending/discard"));
}
#[test]
fn discussion_merges_reviews_by_time_and_marks_stale_and_dismissed_reviews() {
    crate::webui::tests::in_scope(|| {
        let mut stale = review(5, "approved", "2026-10-02T12:30:00+00:00");
        stale.stale = true;
        let mut dismissed = review(6, "dismissed", "2026-10-02T13:30:00+00:00");
        dismissed.dismissed_reason = Some("Superseded".into());
        let comments = CommentPage {
            events_next: None,
            comments: vec![
                comment_at(1, "2026-10-02T12:00:00+00:00"),
                comment_at(2, "2026-10-02T14:00:00+00:00"),
            ],
            reviews: vec![stale, dismissed],
            page: 1,
            has_more: false,
            events: Vec::new(),
        };
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &comments,
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        let at = |id: &str| html.find(id).unwrap_or_else(|| panic!("{id} in the page"));
        assert!(at("id=\"comment-1\"") < at("id=\"review-5\""));
        assert!(at("id=\"review-5\"") < at("id=\"review-6\""));
        assert!(at("id=\"review-6\"") < at("id=\"comment-2\""));
        assert!(html.contains("approved these changes"));
        assert!(html.contains("Stale"));
        assert!(html.contains("Dismissed: Superseded"));
        assert!(html.contains("Please check this one"));
        // Administrators may dismiss a review that is still standing, and only that one.
        assert!(html.contains("/reviews/5/dismiss"));
        assert!(!html.contains("/reviews/6/dismiss"));
    });
}

#[test]
fn sidebar_lists_each_reviewers_latest_decision_and_open_requests() {
    crate::webui::tests::in_scope(|| {
        let reviewers = [
            Reviewer {
                username: "bob".into(),
                status: ReviewerStatus::Approved,
                stale: true,
                pending: false,
                code_owner: false,
            },
            Reviewer {
                username: "carol".into(),
                status: ReviewerStatus::ChangesRequested,
                stale: false,
                pending: false,
                code_owner: false,
            },
            Reviewer {
                username: "dave".into(),
                status: ReviewerStatus::Dismissed,
                stale: false,
                pending: false,
                code_owner: false,
            },
            Reviewer {
                username: "erin".into(),
                status: ReviewerStatus::Requested,
                stale: false,
                pending: true,
                code_owner: false,
            },
        ];
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &reviewers,
        ));
        assert!(html.contains("Reviewers"));
        for label in [
            "Approved",
            "Changes requested",
            "Dismissed",
            "Review requested",
        ] {
            assert!(html.contains(label), "{label}");
        }
        assert!(html.contains("Stale"));
    });
}

#[test]
fn reviewer_forms_are_for_writers_on_open_pull_requests() {
    crate::webui::tests::in_scope(|| {
        let reviewers = [
            Reviewer {
                username: "bob".into(),
                status: ReviewerStatus::Approved,
                stale: false,
                pending: false,
                code_owner: false,
            },
            Reviewer {
                username: "erin".into(),
                status: ReviewerStatus::Requested,
                stale: false,
                pending: true,
                code_owner: false,
            },
        ];
        let render = |repo: &Repository, entry: &Item| {
            dioxus_ssr::render_element(detail(
                repo,
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &reviewers,
            ))
        };
        let html = render(&repository_at(Kind::Pull, "/7"), &item());
        assert!(html.contains("action=\"/alice/repo/pulls/7/requested_reviewers\""));
        assert!(html.contains("/alice/repo/pulls/7/requested_reviewers/erin/remove"));
        assert!(!html.contains("/requested_reviewers/bob/remove"));
        assert!(html.contains("Re-request review"));

        let mut reader = repository_at(Kind::Pull, "/7");
        reader.can_write = false;
        let html = render(&reader, &item());
        assert!(!html.contains("requested_reviewers"));
        assert!(!html.contains("Re-request review"));

        let mut closed = item();
        closed.state = "closed".into();
        let html = render(&repository_at(Kind::Pull, "/7"), &closed);
        assert!(!html.contains("requested_reviewers"));
    });
}

#[test]
fn reviewer_names_and_a_refused_request_are_escaped_and_kept() {
    crate::webui::tests::in_scope(|| {
        let reviewers = [Reviewer {
            username: "ev/il".into(),
            status: ReviewerStatus::Requested,
            stale: false,
            pending: true,
            code_owner: false,
        }];
        let draft = DetailDraft {
            reviewer: "<img src=x>".into(),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &CommentPage::default(),
            &[],
            &draft,
            None,
            &Default::default(),
            &reviewers,
        ));
        assert!(html.contains("/requested_reviewers/ev%2Fil/remove"));
        assert!(!html.contains("<img src=x>"));
        assert!(html.contains("name=\"username\""));
    });
}

#[test]
fn diff_places_threads_under_their_line_and_gutters_open_the_composer() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let mut root = comment_at(5, "2026-10-02T12:00:00+00:00");
    root.body = "Rename this".into();
    root.conversation = Some(Conversation {
        resolved: false,
        outdated: true,
        can_resolve: true,
        commit: "1".repeat(40),
    });
    let thread = Thread {
        root,
        path: "src/lib.rs".into(),
        line: Some(2),
        side: "RIGHT".into(),
        replies: vec![comment_at(6, "2026-10-02T12:10:00+00:00")],
        pending: false,
    };
    let composer = LineComment {
        repository_id: None,
        path: "src/lib.rs".into(),
        side: "LEFT".into(),
        line: "2".into(),
        in_reply_to: None,
        body: "draft words".into(),
        preview: false,
        commit: String::new(),
        base: String::new(),
    };
    let html = dioxus_ssr::render_element(diff_page(
        &repository_at(Kind::Pull, "/7/diff"),
        &item(),
        &diff,
        &[thread],
        true,
        Some(&composer),
        None,
        0,
        None,
        &Default::default(),
        false,
    ));
    let at = |needle: &str| {
        html.find(needle)
            .unwrap_or_else(|| panic!("{needle} in the page"))
    };
    // The composer sits under its row, and the thread under the row it answers.
    assert!(at("keep") < at("draft words"));
    assert!(at("draft words") < at("id=\"thread-5\""));
    assert!(html.contains("Outdated"));
    assert!(html.contains("Rename this"));
    assert!(html.contains("Reply"));
    assert!(html.contains("/alice/repo/pulls/7/comments/5/resolve"));
    assert!(html.contains("side=RIGHT"));
    assert!(html.contains("side=LEFT"));
    assert!(html.contains("line=2#compose"));
    assert!(tag_with(&html, "name=\"side\" value=\"LEFT\"").contains("side"));
    assert!(html.contains("name=\"in_reply_to\" value=\"5\""));
    assert!(html.contains("This pull request has more inline comments"));
}

#[test]
fn a_refused_reply_is_drawn_once_inside_its_thread() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let thread = Thread {
        root: comment_at(5, "2026-10-02T12:00:00+00:00"),
        path: "src/lib.rs".into(),
        line: Some(2),
        side: "RIGHT".into(),
        replies: Vec::new(),
        pending: false,
    };
    let reply = LineComment {
        repository_id: None,
        path: "src/lib.rs".into(),
        side: "RIGHT".into(),
        line: "2".into(),
        in_reply_to: Some(5),
        body: "reply words".into(),
        preview: false,
        commit: String::new(),
        base: String::new(),
    };
    let html = dioxus_ssr::render_element(diff_page(
        &repository_at(Kind::Pull, "/7/diff"),
        &item(),
        &diff,
        &[thread],
        false,
        Some(&reply),
        None,
        0,
        None,
        &Default::default(),
        false,
    ));
    assert_eq!(html.matches("id=\"reply-body-5\"").count(), 1);
    assert!(html.contains("reply words"));
    assert!(!html.contains("id=\"line-comment-body\""));
}

#[test]
fn a_closed_pull_request_shows_the_refused_review_text_read_only() {
    // The read-only field is a hook-using component, so the render runs in a component scope.
    crate::webui::tests::in_scope(|| {
        let mut closed = item();
        closed.state = "closed".into();
        let draft = ReviewDraft {
            state: "approved".into(),
            body: "Kept closed text".into(),
            preview: false,
            commit: String::new(),
        };
        let repo = repository_at(Kind::Pull, "/7/diff");
        let html = files_html(&repo, &closed, Some(&draft), 0);
        assert!(html.contains("This pull request is closed; your text was not saved."));
        assert!(html.contains("Kept closed text"));
        assert!(html.contains("readonly"));
        assert!(!html.contains("id=\"review-form\""));

        // The bootstrap pre-renders pages, which wraps dynamic text in comment markers. The field
        // must hold its text without them.
        let mut renderer = dioxus_ssr::Renderer::new();
        renderer.pre_render = true;
        let pre = renderer.render_element(diff_page(
            &repo,
            &closed,
            &files_diff(),
            &[],
            false,
            None,
            Some(&draft),
            0,
            None,
            &Default::default(),
            false,
        ));
        let at = pre.find("Kept closed text").unwrap();
        let start = pre[..at].rfind("<textarea").unwrap();
        let end = at + pre[at..].find("</textarea>").unwrap();
        assert!(!pre[start..end].contains("<!--"), "{}", &pre[start..end]);
    });
}
#[test]
fn list_tabs_count_each_state_and_the_search_is_a_plain_get_form() {
    crate::webui::tests::in_scope(|| {
        let repo = repository();
        let counts = ListCounts {
            open: 3,
            closed: 1,
            all: 4,
        };
        let html =
            dioxus_ssr::render_element(list(&repo, Kind::Issue, "open", "crash", counts, &[]));
        let content = html.split_once("class=\"repo-content ").unwrap().1;
        // The state toggles show their counts in their text.
        assert!(content.contains(">3 Open</span>"));
        assert!(content.contains(">1 Closed</span>"));
        // Tab links keep the search, so changing the state does not drop it.
        assert!(content.contains("state=closed"));
        assert!(content.contains("q=crash"));
        let form = content
            .split_once("class=\"collaboration-search\"")
            .unwrap()
            .1
            .split_once("</form>")
            .unwrap()
            .0;
        assert!(form.contains("method=\"get\""));
        assert!(form.contains("name=\"q\""));
        // The box shows the state as a qualifier, then the search words.
        assert!(form.contains("value=\"is:issue state:open crash\""));
        assert!(form.contains("maxlength=\"200\""));
        assert!(!form.contains("method=\"post\""));
    });
}

#[test]
fn empty_search_results_name_the_search_and_offer_a_way_out() {
    assert_eq!(
        empty_title(Kind::Issue, "open", "crash"),
        "No open issues match \"crash\""
    );
    assert_eq!(
        empty_title(Kind::Pull, "closed", "docs"),
        "No closed pull requests match \"docs\""
    );
    assert_eq!(
        empty_title(Kind::Issue, "all", "x"),
        "No issues match \"x\""
    );
    assert_eq!(empty_title(Kind::Issue, "open", ""), "No open issues");
    crate::webui::tests::in_scope(|| {
        let repo = repository_at(Kind::Pull, "");
        let html = dioxus_ssr::render_element(list(
            &repo,
            Kind::Pull,
            "open",
            "docs",
            ListCounts::default(),
            &[],
        ));
        let content = html.split_once("class=\"repo-content ").unwrap().1;
        assert!(content.contains("Clear search"));
        assert!(!content.contains("View all"));
    });
}

/// The html from the composer's action to the end of its form.
/// A workflow run with the given ID and status, as the Checks tab lists it.
fn run_fixture(id: &str, status: &str) -> Run {
    Run {
        id: id.into(),
        repository: "repo".into(),
        status: status.into(),
        workflow_path: ".github/workflows/ci.yml".into(),
        commit_status_context: "gitserv/ci".into(),
        branch: "topic".into(),
        event: "pull_request".into(),
        pipeline: "ci".into(),
        mode: "act".into(),
        head_sha: Some("a".repeat(40)),
        created_at: 1,
        started_at: None,
        completed_at: None,
        error_message: None,
        inputs: Default::default(),
    }
}

#[test]
fn pull_tabs_count_commits_and_checks_and_mark_only_the_page_shown() {
    crate::webui::tests::in_scope(|| {
        let mut shown = item();
        shown.comment_count = 2;
        shown.commit_count = Some(3);
        shown.check_count = 1;
        shown.file_count = Some(4);
        let tabs = |pull: &Item, active: detail::PullTab| {
            dioxus_ssr::render_element(detail::pull_tabs("/alice/repo/pulls/7", pull, active))
        };
        let commits = tabs(&shown, detail::PullTab::Commits);
        assert_eq!(commits.matches("aria-current=\"page\"").count(), 1);
        assert!(commits.contains("href=\"/alice/repo/pulls/7/commits\""));
        assert!(commits.contains("href=\"/alice/repo/pulls/7/checks\""));
        assert!(commits.contains("<span class=\"collaboration-tab-count\">2</span>"));
        assert!(commits.contains("<span class=\"collaboration-tab-count\">3</span>"));
        assert!(commits.contains("<span class=\"collaboration-tab-count\">1</span>"));
        // A phone reaches Commits and Checks through the More menu, and Files changed stays in the strip.
        assert_eq!(
            commits
                .matches("href=\"/alice/repo/pulls/7/commits\"")
                .count(),
            2
        );
        assert!(commits.contains("aria-label=\"More pull request pages\""));
        assert!(tabs(&shown, detail::PullTab::Files)
            .contains("Files changed</span><span class=\"collaboration-tab-count\">4</span>"));
        shown.commit_count = Some(100);
        assert!(tabs(&shown, detail::PullTab::Files).contains("100+"));
        // Without a count from Git, the Commits tab shows no number at all. The Conversation, Checks
        // and Files changed tabs still show theirs.
        shown.commit_count = None;
        assert_eq!(
            tabs(&shown, detail::PullTab::Files)
                .matches("collaboration-tab-count")
                .count(),
            3
        );
    });
}

#[test]
fn pr_commits_tab_lists_each_commit_with_a_copy_button_and_marks_its_tab() {
    crate::webui::tests::in_scope(|| {
        let mut shown = item();
        shown.commit_count = Some(1);
        let commit = Commit {
            sha: "a".repeat(40),
            message: "Add the parser\n\nDetails below.".into(),
            author: "bob".into(),
            author_date: "2026-10-01T10:00:00+00:00".into(),
            committer: "bob".into(),
            committer_date: "2026-10-01T10:00:00+00:00".into(),
            parents: vec!["b".repeat(40)],
        };
        let html = dioxus_ssr::render_element(render(Page::PullCommits {
            repository: repository_at(Kind::Pull, "/7/commits"),
            item: shown,
            commits: vec![commit],
        }));
        assert_eq!(html.matches("class=\"repo-commit-row\"").count(), 1);
        assert!(html.contains("Add the parser"));
        assert!(html.contains("Copy SHA"));
        assert!(html.contains(&format!("/alice/repo/commit/{}", "a".repeat(40))));
        assert!(html.contains("<span aria-current=\"page\">Commits</span>"));
    });
}

#[test]
fn pr_checks_tab_lists_each_run_as_an_actions_row_linked_to_its_logs() {
    crate::webui::tests::in_scope(|| {
        let mut shown = item();
        shown.check_count = 1;
        let html = dioxus_ssr::render_element(render(Page::PullChecks {
            repository: repository_at(Kind::Pull, "/7/checks"),
            item: shown.clone(),
            runs: vec![run_fixture("real-run", "success")],
            statuses: Vec::new(),
        }));
        assert_eq!(html.matches("class=\"actions-run-row\"").count(), 1);
        assert!(html.contains("/alice/repo/actions?job=real-run"));
        assert!(html.contains("class=\"actions-page\""));
        let empty = dioxus_ssr::render_element(render(Page::PullChecks {
            repository: repository_at(Kind::Pull, "/7/checks"),
            item: shown,
            runs: Vec::new(),
            statuses: Vec::new(),
        }));
        assert!(empty.contains("No workflow runs have started on this commit."));
        assert!(!empty.contains("actions-run-row"));
        // A writer can add the workflow file on the head branch, where the checks run.
        assert!(empty.contains("href=\"/alice/repo/new/refs/heads/topic/.github/workflows\""));
        let mut reader = repository_at(Kind::Pull, "/7/checks");
        reader.can_write = false;
        let read_only = dioxus_ssr::render_element(render(Page::PullChecks {
            repository: reader,
            item: item(),
            runs: Vec::new(),
            statuses: Vec::new(),
        }));
        assert!(read_only.contains("No workflow runs have started on this commit."));
        assert!(!read_only.contains(".github/workflows"));
        let external = dioxus_ssr::render_element(render(Page::PullChecks {
            repository: repository_at(Kind::Pull, "/7/checks"),
            item: item(),
            runs: Vec::new(),
            statuses: vec![crate::webui::pages::repos::pulls::CheckStatus {
                context: "external/compiler".into(),
                state: "failure".into(),
                description: Some("<script>blocked</script>".into()),
                target_url: Some("javascript:alert(1)".into()),
            }],
        }));
        assert!(external.contains("external/compiler"));
        assert!(external.contains("failure"));
        assert!(!external.contains("javascript:"));
        assert!(!external.contains("<script>blocked"));
        assert!(!external.contains("No workflow runs have started"));
    });
}

#[test]
fn sidebar_label_picker_is_320px_with_colour_dots_and_wrapping_descriptions() {
    let css = include_str!("../style.css");
    let menu = css
        .split_once(".collaboration-picker-menu { position: absolute;")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(
        menu.contains("width: min(320px, calc(100vw - 32px));"),
        "{menu}"
    );
    let description = css
        .split_once(".collaboration-picker-description {")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(
        !description.contains("nowrap") && !description.contains("ellipsis"),
        "{description}"
    );
    assert!(css.contains(".collaboration-picker-dot { display: inline-block; flex: none;"));
}

#[test]
fn pull_heading_on_a_phone_stacks_title_state_and_one_action_row() {
    let css = include_str!("../style.css");
    assert!(
        css.contains(".collaboration-detail-heading > h2 { grid-column: 1 / -1; grid-row: 1; }")
    );
    assert!(css.contains(
        ".collaboration-detail-heading > .collaboration-heading-action { grid-column: 1; grid-row: 3; justify-self: start; }"
    ));
    assert!(css.contains(
        ".collaboration-detail-heading > .collaboration-edit { grid-column: 2; grid-row: 3; justify-self: end; }"
    ));
}

#[test]
fn state_pills_are_solid_with_white_text_in_both_themes() {
    let css = include_str!("../style.css");
    assert!(css.contains("--collaboration-fill-open: #238636;"));
    assert!(css.contains(".collaboration-state-open { border-color: var(--collaboration-fill-open); background: var(--collaboration-fill-open); color: #fff; }"));
    assert!(css.contains(".collaboration-state > .collaboration-item-icon { color: inherit; }"));
}

#[test]
fn pull_request_rows_name_their_review_and_checks_states() {
    crate::webui::tests::in_scope(|| {
        let mut approved = item();
        approved.review_state = Some("approved".into());
        approved.check_state = Some("failure".into());
        let mut changed = item();
        changed.number = 8;
        changed.review_state = Some("changes_requested".into());
        changed.check_state = Some("pending".into());
        let html = dioxus_ssr::render_element(list(
            &repository(),
            Kind::Pull,
            "open",
            "",
            ListCounts::default(),
            &[approved, changed],
        ));
        for label in [
            "Approved",
            "Changes requested",
            "Checks failed",
            "Checks running",
        ] {
            assert!(
                html.contains(&format!("title=\"{label}\"")),
                "missing {label}"
            );
        }
    });
}

fn composer_html(html: &str) -> &str {
    let start = html
        .find("action=\"/alice/repo/pulls/7/comments\"")
        .expect("a composer");
    let rest = &html[start..];
    &rest[..rest.find("</form>").unwrap()]
}

#[test]
fn one_composer_comments_alone_and_reviews_live_on_the_files_page() {
    crate::webui::tests::in_scope(|| {
        let render = |repo: &Repository, entry: &Item| {
            dioxus_ssr::render_element(detail(
                repo,
                Kind::Pull,
                entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
            ))
        };
        let writer = repository_at(Kind::Pull, "/7");
        let mut reader = repository_at(Kind::Pull, "/7");
        reader.can_write = false;
        reader.can_admin = false;
        reader.chrome.can_write = false;
        reader.chrome.can_admin = false;
        let mut signed_out = repository_at(Kind::Pull, "/7");
        signed_out.signed_in = false;
        signed_out.chrome.signed_in = false;

        // The comment box is the only composer here. Reviews are written on the Files changed page.
        let html = render(&writer, &item());
        let composer = composer_html(&html);
        assert_eq!(
            html.matches("action=\"/alice/repo/pulls/7/comments\"")
                .count(),
            1
        );
        assert!(composer.contains("Add a comment</h3>"));
        assert!(!composer.contains("id=\"review-form\""));
        assert!(!composer.contains("name=\"state\""));
        assert!(!composer.contains("formaction="));
        assert!(composer.contains(">Comment</button>"));
        assert!(!composer.contains(">Approve</button>"));

        // A reader gets the same comment box, and the text box is required.
        let mut own = item();
        own.comparison.as_mut().unwrap().can_vote = false;
        let html = render(&reader, &own);
        let composer = composer_html(&html);
        assert!(composer.contains("Add a comment</h3>"));
        assert!(!composer.contains("name=\"state\""));
        assert!(composer.contains(">Comment</button>"));
        assert!(composer.contains("required"));

        // Signed out, the page asks people to sign in and has no composer.
        let html = render(&signed_out, &item());
        assert!(!html.contains("action=\"/alice/repo/pulls/7/comments\""));
        assert!(html.contains("to join the discussion."));

        // A closed pull request keeps the comment box and Reopen, with no vote and no review anchor.
        let mut closed = item();
        closed.state = "closed".into();
        let html = render(&writer, &closed);
        let composer = composer_html(&html);
        assert!(!composer.contains("id=\"review-form\""));
        assert!(!composer.contains("name=\"state\""));
        assert!(!composer.contains("formaction="));
        assert!(composer.contains(">Comment</button>"));
        assert!(composer.contains(">Reopen pull request</button>"));

        // A merged pull request has neither the vote nor a state button.
        let mut merged = item();
        merged.state = "merged".into();
        let html = render(&writer, &merged);
        let composer = composer_html(&html);
        assert!(!composer.contains("name=\"state\""));
        assert!(!composer.contains("Reopen pull request"));
        assert!(composer.contains(">Comment</button>"));
    });
}

#[test]
fn files_list_each_file_and_offer_viewed_marks_to_signed_in_viewers() {
    crate::webui::tests::in_scope(|| {
        let files = || {
            vec![
                DiffFile {
                    filename: "src/lib.rs".into(),
                    status: "modified".into(),
                    additions: 2,
                    deletions: 1,
                    patch: "@@ -1 +1,2 @@\n-old\n+new\n+more\n".into(),
                    owners: Vec::new(),
                },
                DiffFile {
                    filename: "docs/new.md".into(),
                    status: "added".into(),
                    additions: 3,
                    deletions: 0,
                    patch: "@@ -0,0 +1,3 @@\n+a\n+b\n+c\n".into(),
                    owners: Vec::new(),
                },
            ]
        };
        let marks = Viewed {
            csrf: "csrf-sentinel".into(),
            action: "/acme/widgets/pulls/7/viewed".into(),
            head: "head-sentinel".into(),
            base: "base-sentinel".into(),
            paths: ["docs/new.md".to_string()].into_iter().collect(),
        };
        let diff = Diff {
            files: files(),
            additions: 5,
            deletions: 1,
            viewed: Some(marks),
        };
        let html = dioxus_ssr::render_element(diff_page(
            &repository_at(Kind::Pull, "/7/diff"),
            &item(),
            &diff,
            &[],
            false,
            None,
            None,
            0,
            None,
            &Default::default(),
            false,
        ));
        assert!(html.contains("aria-label=\"Changed files\""));
        assert!(html.contains("href=\"#diff-file-0\""));
        assert!(html.contains("href=\"#diff-file-1\""));
        assert!(html.contains("id=\"diff-file-0\""));
        assert!(html.contains("collaboration-file-modified"));
        assert!(html.contains("collaboration-file-added"));
        // The count comes from the server: one of two files is viewed.
        assert!(html.contains("collaboration-viewed-done\">1</span> / 2 files viewed"));
        // Every file has a Viewed button that submits its own form, with the CSRF token and path.
        assert_eq!(html.matches("role=\"checkbox\"").count(), 2);
        assert!(html.contains("form=\"viewed-form-0\""));
        assert!(html.contains("form=\"viewed-form-1\""));
        assert!(html.contains("action=\"/acme/widgets/pulls/7/viewed\""));
        assert!(html.contains("value=\"csrf-sentinel\""));
        assert!(html.contains("value=\"docs/new.md\""));
        // The viewed file is drawn collapsed and checked; the other one is open and unchecked.
        assert!(!tag_with(&html, "id=\"diff-file-1\"").contains(" open"));
        assert!(tag_with(&html, "id=\"diff-file-0\"").contains(" open"));
        assert_eq!(html.matches("aria-checked=\"true\"").count(), 1);
        assert_eq!(html.matches("aria-checked=\"false\"").count(), 1);

        // A signed-out reader sees the files and no Viewed marks.
        let signed_out = Diff {
            files: files(),
            additions: 5,
            deletions: 1,
            viewed: None,
        };
        let html = dioxus_ssr::render_element(diff_page(
            &repository_at(Kind::Pull, "/7/diff"),
            &item(),
            &signed_out,
            &[],
            false,
            None,
            None,
            0,
            None,
            &Default::default(),
            false,
        ));
        assert!(html.contains("href=\"#diff-file-0\""));
        assert!(!html.contains("role=\"checkbox\""));
        assert!(!html.contains("files viewed"));

        // A branch comparison has the same list and no marks, because it is not a pull request.
        let html = dioxus_ssr::render_element(compare(
            &repository_at(Kind::Pull, "/compare"),
            "main",
            "topic",
            &["main".into(), "topic".into()],
            Some(&signed_out),
            None,
            false,
        ));
        assert!(html.contains("href=\"#diff-file-1\""));
        assert!(!html.contains("role=\"checkbox\""));
    });
}

/// The rows of a patch, as the Files changed list and a commit page draw them.
#[test]
fn diff_rows_keep_their_text_and_numbers_and_hide_git_headers() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "file".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "--- a/file\n+++ b/file\n@@ -1,2 +1,2 @@\n context\n-old\n+<script>new</script>\n\\ No newline at end of file".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let html = dioxus_ssr::render_element(diff_view(&diff, false));
    // The file headers are not rows: the file's own header names the file.
    assert!(!html.contains("--- a/file"), "{html}");
    assert!(!html.contains("+++ b/file"), "{html}");
    assert!(
        html.contains("<span class=\"patch-line patch-hunk\">@@ -1,2 +1,2 @@\n</span>"),
        "{html}"
    );
    // Context and changed lines carry their old and new line numbers, and their text is escaped.
    assert!(html.contains("data-old=\"1\" data-new=\"1\""), "{html}");
    assert!(
        html.contains("<span class=\"patch-line\"> context\n</span>"),
        "{html}"
    );
    assert!(html.contains("data-old=\"2\""), "{html}");
    assert!(
        html.contains("<span class=\"patch-line patch-deletion\">-old\n</span>"),
        "{html}"
    );
    assert!(html.contains("data-new=\"2\""), "{html}");
    assert!(
        html.contains(
            "<span class=\"patch-line patch-addition\">+&#60;script&#62;new&#60;/script&#62;\n</span>"
        ),
        "{html}"
    );
    // The marker that follows a line has no numbers, and it keeps its text.
    assert!(
        html.contains("<span class=\"patch-line\">\\ No newline at end of file</span>"),
        "{html}"
    );
}

#[test]
fn diff_lines_offer_a_plus_that_opens_the_composer_to_signed_in_readers() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "@@ -1,2 +1,2 @@\n keep\n-old\n+new".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let repo = repository_at(Kind::Pull, "/7/diff");
    let page = |split: bool| {
        dioxus_ssr::render_element(diff_page(
            &repo,
            &item(),
            &diff,
            &[],
            false,
            None,
            None,
            0,
            None,
            &Default::default(),
            split,
        ))
    };
    // A context line is commented on at its new number and a removed line at its old one. The
    // hunk header has no numbers, so it has no "+".
    let unified = page(false);
    assert_eq!(
        unified.matches("<a class=\"patch-add\"").count(),
        3,
        "{unified}"
    );
    assert!(unified.contains("side=RIGHT"), "{unified}");
    assert!(unified.contains("side=LEFT"), "{unified}");
    assert!(
        unified.contains("aria-label=\"Comment on line 2\""),
        "{unified}"
    );
    // The split view has a "+" on each side of a numbered row.
    let split = page(true);
    assert_eq!(
        split.matches("<a class=\"patch-add\"").count(),
        4,
        "{split}"
    );
    assert!(split.contains("view=split#compose"), "{split}");
    // Without a composer the cells are still there, empty, so the columns line up.
    let signed_out = dioxus_ssr::render_element(diff_view(&diff, false));
    assert!(
        !signed_out.contains("<a class=\"patch-add\""),
        "{signed_out}"
    );
    assert!(
        signed_out.contains("<span class=\"patch-add\"></span>"),
        "{signed_out}"
    );
}

#[test]
fn files_changed_puts_its_tree_behind_a_toggle_and_draws_no_script_only_controls() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: "@@ -1 +1,2 @@\n-x\n+y\n+z\n".into(),
            owners: Vec::new(),
        }],
        additions: 2,
        deletions: 1,
        viewed: None,
    };
    let html = dioxus_ssr::render_element(diff_view(&diff, false));
    assert!(html.contains("1 file changed"), "{html}");
    // The toggle starts open, which shows the tree on a wide screen. A phone hides the tree while
    // the toggle is open, so its Files button starts collapsed there.
    assert!(
        tag_with(&html, "class=\"collaboration-diff-tree-toggle\"").contains(" open"),
        "{html}"
    );
    assert!(
        html.contains("<aside class=\"collaboration-diff-tree\""),
        "{html}"
    );
    // Expand and collapse all need the browser, so the page without scripts does not draw them.
    assert!(!html.contains("Expand all files"), "{html}");
    assert!(!html.contains("Collapse all files"), "{html}");
}

fn milestone(number: i32, title: &str, open: i64, closed: i64) -> Milestone {
    Milestone {
        id: i64::from(number),
        number,
        title: title.into(),
        description: Some("Ship it".into()),
        due_date: Some("2026-10-20".into()),
        state: "open".into(),
        open_items: open,
        closed_items: closed,
    }
}

#[test]
fn timeline_events_show_a_badge_for_their_kind_and_threads_link_to_the_files_tab() {
    crate::webui::tests::in_scope(|| {
        let event = |kind: &str, detail: &str| super::Event {
            actor: "alice".into(),
            kind: kind.into(),
            detail: detail.into(),
            previous: String::new(),
            created: "2026-10-02T12:00:00+00:00".into(),
        };
        let mut thread = comment_at(3, "2026-10-02T12:00:00+00:00");
        thread.location = Some("src/main.rs:1".into());
        thread.conversation = Some(Conversation {
            resolved: false,
            outdated: false,
            can_resolve: false,
            commit: "abc".into(),
        });
        let comments = CommentPage {
            events_next: None,
            comments: vec![thread],
            reviews: Vec::new(),
            page: 1,
            has_more: false,
            events: vec![
                event("assigned", "bob"),
                event("labeled", "bug"),
                event("closed", ""),
            ],
        };
        let html = dioxus_ssr::render_element(detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &comments,
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ));
        for title in ["Assigned", "Label added", "Closed"] {
            assert!(html.contains(&format!("title=\"{title}\"")), "{html}");
        }
        assert_eq!(
            html.matches("class=\"collaboration-event-icon\"").count(),
            3
        );
        // The thread's location links to the thread on the Files changed tab, not the internal side.
        assert!(
            html.contains("href=\"/alice/repo/pulls/7/diff#thread-3\""),
            "{html}"
        );
        assert!(html.contains("src/main.rs:1"));
        assert!(!html.contains("(RIGHT)"));
    });
}

#[test]
fn reviewer_picker_leaves_out_the_author_of_the_pull_request() {
    crate::webui::tests::in_scope(|| {
        let mut pull = item();
        pull.author = "alice".into();
        let assignable = vec!["alice".to_string(), "bob".to_string()];
        let html = dioxus_ssr::render_element(super::detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &pull,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
            &assignable,
            &[],
            &[],
        ));
        let start = html.find("Request reviews from").unwrap();
        let picker = &html[start..html.find("Request a review from").unwrap()];
        assert!(picker.contains("src=\"/avatars/bob\""), "{picker}");
        assert!(!picker.contains("value=\"alice\""), "{picker}");
        assert!(!picker.contains("/avatars/alice"), "{picker}");
    });
}

#[test]
fn new_label_opens_a_panel_under_its_trigger_with_a_cancel_link() {
    crate::webui::tests::in_scope(|| {
        let html = dioxus_ssr::render_element(labels::list(
            &repository(),
            &[],
            "",
            "name",
            &Default::default(),
            1,
            false,
        ));
        assert!(
            html.contains("native-menu collaboration-label-new"),
            "{html}"
        );
        assert!(html.contains("collaboration-label-new-panel"), "{html}");
        assert!(html.contains(">Cancel</a>"), "{html}");
    });
}

#[test]
fn labels_sort_by_most_used_and_count_the_items_on_each_label() {
    crate::webui::tests::in_scope(|| {
        let label = |id: i64, name: &str| Label {
            id,
            name: name.into(),
            color: "#0075ca".into(),
            description: String::new(),
        };
        let items = vec![label(1, "wontfix"), label(2, "release")];
        let usage = std::collections::BTreeMap::from([(1, 1), (2, 3)]);
        let html = dioxus_ssr::render_element(labels::list(
            &repository(),
            &items,
            "",
            "most-used",
            &usage,
            1,
            false,
        ));
        assert!(html.contains("2 labels"), "{html}");
        assert!(html.contains("3 issues and pull requests"), "{html}");
        assert!(html.contains("Sort: Most used"), "{html}");
        let release = html.find(">release<").unwrap_or_else(|| panic!("{html}"));
        let wontfix = html.find(">wontfix<").unwrap_or_else(|| panic!("{html}"));
        assert!(release < wontfix, "{html}");
    });
}

#[test]
fn label_delete_asks_in_a_centred_dialog_with_cancel() {
    crate::webui::tests::in_scope(|| {
        let html = dioxus_ssr::render_element(labels::list(
            &repository(),
            &[Label {
                id: 1,
                name: "wontfix".into(),
                color: "#0075ca".into(),
                description: String::new(),
            }],
            "",
            "name",
            &Default::default(),
            1,
            false,
        ));
        assert!(
            html.contains("aria-label=\"Delete label wontfix\""),
            "{html}"
        );
        assert!(
            html.contains("action=\"/alice/repo/_forms/delete-label\""),
            "{html}"
        );
        assert!(html.contains("href=\"/alice/repo/labels\""), "{html}");
        assert!(html.contains(">Cancel</a>"), "{html}");
        assert!(html.contains("name=\"confirm\""), "{html}");
        assert!(!html.contains("class=\"collaboration-checkbox\""), "{html}");
    });
}

#[test]
fn sidebar_lists_each_participant_once_and_skips_deleted_accounts() {
    crate::webui::tests::in_scope(|| {
        let comment = |id: i64, author: &str| Comment {
            revision: "comment-revision".into(),
            id,
            author: author.into(),
            body: "Thanks".into(),
            created: "2026-10-01T00:00:00Z".into(),
            location: None,
            conversation: None,
            reactions: Vec::new(),
            can_edit: false,
            can_delete: false,
            edited: None,
            edit: None,
        };
        let comments = CommentPage {
            events_next: None,
            comments: vec![
                comment(1, "bob"),
                comment(2, "Deleted user"),
                comment(3, "alice"),
                comment(4, "bob"),
                comment(5, "carol"),
            ],
            page: 1,
            has_more: false,
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(super::detail(
            &repository(),
            Kind::Issue,
            &item(),
            &comments,
            &[],
            &DetailDraft::default(),
            None,
            &References::default(),
            &[],
            &[],
            &[],
            &[],
        ));
        // The discussion links every author as well, so only the sidebar list is checked.
        let start = html
            .find("class=\"collaboration-participants\"")
            .unwrap_or_else(|| panic!("{html}"));
        let list = &html[start..];
        let list = &list[..list.find("</ul>").unwrap_or(list.len())];
        for name in ["alice", "bob", "carol"] {
            let link = format!("href=\"/{name}\"");
            assert_eq!(list.matches(&link).count(), 1, "{list}");
        }
        assert!(!list.contains("avatars/Deleted"), "{list}");
    });
}

#[test]
fn sidebar_offers_assignees_and_milestones_to_writers_and_only_shows_them_to_readers() {
    crate::webui::tests::in_scope(|| {
        let shown = Item {
            assignees: vec!["bob".into()],
            milestone: Some(milestone(1, "v1", 1, 1)),
            ..item()
        };
        let mut reader = repository();
        reader.can_write = false;
        let assignable = vec!["carol".to_owned()];
        let milestones = vec![milestone(1, "v1", 1, 1)];
        let writer_html = dioxus_ssr::render_element(super::detail(
            &repository(),
            Kind::Issue,
            &shown,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &References::default(),
            &[],
            &[],
            &assignable,
            &milestones,
        ));
        for expected in [
            "action=\"/alice/repo/issues/7/assignees\"",
            "value=\"carol\"",
            "action=\"/alice/repo/issues/7/assignees/bob/remove\"",
            "action=\"/alice/repo/issues/7/milestone\"",
            "name=\"milestone_number\"",
        ] {
            assert!(writer_html.contains(expected), "missing {expected}");
        }
        let reader_html = dioxus_ssr::render_element(super::detail(
            &reader,
            Kind::Issue,
            &shown,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &References::default(),
            &[],
            &[],
            &[],
            &[],
        ));
        assert!(reader_html.contains("collaboration-assignee\""));
        assert!(reader_html.contains("href=\"/alice/repo/milestones/1\""));
        for hidden in [
            "/assignees\"",
            "/assignees/bob/remove",
            "/milestone\"",
            "name=\"username\"",
        ] {
            assert!(!reader_html.contains(hidden), "reader sees {hidden}");
        }
        // A pull request has the same assignee and milestone forms as an issue.
        let pull_html = dioxus_ssr::render_element(super::detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &Item {
                milestone: None,
                ..item()
            },
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &References::default(),
            &[],
            &[],
            &assignable,
            &milestones,
        ));
        assert!(pull_html.contains("action=\"/alice/repo/pulls/7/assignees\""));
        assert!(pull_html.contains("action=\"/alice/repo/pulls/7/milestone\""));
    });
}

#[test]
fn milestone_pages_show_progress_rows_and_writer_forms() {
    crate::webui::tests::in_scope(|| {
        let release = milestone(3, "Release", 1, 3);
        let list_html = dioxus_ssr::render_element(milestones::list(
            &repository(),
            "open",
            ListCounts {
                open: 1,
                closed: 0,
                all: 1,
            },
            std::slice::from_ref(&release),
            "",
            1,
            false,
        ));
        for expected in [
            "role=\"progressbar\"",
            "width: 75%",
            "href=\"/alice/repo/milestones/3\"",
            "href=\"/alice/repo/milestones/new\"",
            "Due ",
        ] {
            assert!(list_html.contains(expected), "missing {expected}");
        }
        let detail_html = dioxus_ssr::render_element(milestones::detail(
            &repository(),
            &release,
            "all",
            &[item()],
            &[],
            (2, true),
            &MilestoneDraft {
                repository_id: None,
                title: "Release".into(),
                description: "Ship it".into(),
                due_date: "2026-10-20".into(),
            },
            None,
        ));
        let detail_html = detail_html
            .replace("&#38;", "&")
            .replace("&#x26;", "&")
            .replace("&amp;", "&");
        for expected in [
            "action=\"/alice/repo/milestones/3/edit\"",
            "action=\"/alice/repo/milestones/3/close\"",
            "action=\"/alice/repo/milestones/3/delete\"",
            // Deleting opens a dialog with a ticked confirmation box, as label deletion does.
            "name=\"confirm\"",
            "repo-confirm-modal",
            "aria-label=\"Delete milestone Release\"",
            "href=\"/alice/repo/milestones/3\"",
            ">Cancel</a>",
            "href=\"/alice/repo/milestones/3?state=all\"",
            "class=\"collaboration-row\"",
            "← Milestones",
            "collaboration-list-header",
            "href=\"/alice/repo/milestones/3?state=all&page=1\"",
            "href=\"/alice/repo/milestones/3?state=all&page=3\"",
        ] {
            assert!(
                detail_html.contains(expected),
                "missing {expected}: {detail_html}"
            );
        }
        assert!(!detail_html.contains("class=\"collaboration-checkbox\""));
        let mut reader = repository();
        reader.can_write = false;
        let reader_html = dioxus_ssr::render_element(milestones::detail(
            &reader,
            &release,
            "all",
            &[],
            &[],
            (1, false),
            &MilestoneDraft::default(),
            None,
        ));
        assert!(!reader_html.contains("/milestones/3/edit"));
        assert!(!reader_html.contains("/milestones/3/delete"));
    });
}

#[test]
fn list_filters_are_native_menus_and_tabs_keep_them() {
    crate::webui::tests::in_scope(|| {
        let filters = ListFilters {
            labels: ["bug", "ui", "docs"]
                .iter()
                .enumerate()
                .map(|(id, name)| Label {
                    id: id as i64,
                    name: (*name).into(),
                    color: "d73a4a".into(),
                    description: String::new(),
                })
                .collect(),
            selected_labels: vec!["bug".into(), "docs".into()],
            assignees: vec!["bob".into()],
            assignee: "bob".into(),
            milestones: vec![milestone(3, "Release", 1, 0)],
            milestone: "3".into(),
            authors: vec!["alice".into()],
            author: "alice".into(),
            sort: "oldest".into(),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(super::list(
            &repository(),
            Kind::Issue,
            "open",
            "",
            ListCounts::default(),
            &[],
            &filters,
        ));
        for expected in [
            // The search box holds the state and every filter as qualifiers.
            "method=\"get\"",
            "name=\"q\"",
            "value=\"is:issue state:open author:alice assignee:bob label:bug label:docs milestone:Release sort:oldest\"",
            // Each menu is a titled popover with its own search field.
            "Filter by author",
            "Filter by label",
            "Filter by milestone",
            "Filter by assignee",
            "Sort by",
            "aria-label=\"Filter labels\"",
            // Each filter is a native menu named by its text, and a menu with a choice set is marked.
            ">Author</span>",
            ">Labels</span>",
            ">Milestones</span>",
            ">Assignees</span>",
            ">Sort: Oldest</span>",
            "collaboration-filter-set",
            "collaboration-filter-chosen",
            // Each choice is a link that applies that choice, and keeps the others.
            "Recently updated",
            "Most commented",
            "label=bug",
            "label=docs",
            "assignee=bob",
            "milestone=3",
            "sort=oldest",
            ">Release</span>",
            ">alice</span>",
        ] {
            assert!(html.contains(expected), "missing {expected}");
        }
        // A pull list offers the same filters as an issue list, even with no labels to choose.
        let pull_filters = ListFilters {
            labels: Vec::new(),
            selected_labels: Vec::new(),
            ..filters.clone()
        };
        let pulls = dioxus_ssr::render_element(super::list(
            &repository_at(Kind::Pull, ""),
            Kind::Pull,
            "open",
            "",
            ListCounts::default(),
            &[],
            &pull_filters,
        ));
        assert!(pulls.contains("assignee:bob"));
        assert!(pulls.contains("milestone:Release"));
        assert!(pulls.contains("value=\"is:pr state:open"));
        assert!(pulls.contains("<span>Labels</span>"));
        assert!(pulls.contains("No labels yet"));
    });
}

#[test]
fn role_badges_name_the_author_and_owner_and_skip_deleted_accounts() {
    use super::detail::role_badge;
    assert_eq!(role_badge("alice", "bob", "bob"), Some("Author"));
    assert_eq!(role_badge("alice", "bob", "alice"), Some("Owner"));
    assert_eq!(role_badge("alice", "bob", "carol"), None);
    assert_eq!(role_badge("alice", "Deleted user", "Deleted user"), None);
}

#[test]
fn label_filter_choices_add_or_remove_one_label() {
    use super::list::toggle_label;
    let filters = ListFilters {
        selected_labels: vec!["bug".into()],
        ..Default::default()
    };
    let added = toggle_label(&filters, "docs");
    assert_eq!(added.selected_labels, ["bug", "docs"]);
    let removed = toggle_label(&filters, "bug");
    assert!(removed.selected_labels.is_empty());
}

#[test]
fn new_issue_offers_assignees_labels_and_a_milestone_to_writers_only() {
    let choices = IssueChoices {
        assignable: vec!["bob".into()],
        labels: vec![Label {
            id: 3,
            name: "bug".into(),
            color: "d73a4a".into(),
            description: String::new(),
        }],
        milestones: vec![Milestone {
            id: 1,
            number: 1,
            title: "v1".into(),
            description: None,
            due_date: None,
            state: "open".into(),
            open_items: 0,
            closed_items: 0,
        }],
    };
    let draft = NewDraft {
        title: "Crash".into(),
        assignees: vec!["bob".into()],
        labels: vec!["3".into()],
        milestone: "1".into(),
        ..Default::default()
    };
    let render_for = |can_write: bool, can_admin: bool| {
        let mut repo = repository_at(Kind::Issue, "/new");
        repo.can_write = can_write;
        repo.can_admin = can_admin;
        crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(render(Page::New {
                repository: repo,
                kind: Kind::Issue,
                draft: draft.clone(),
                branches: BranchChoices::default(),
                choices: if can_write {
                    choices.clone()
                } else {
                    IssueChoices::default()
                },
                diff: None,
                error: None,
                preview: false,
                references: Default::default(),
            }))
        })
    };
    let writer = render_for(true, true);
    assert!(writer.contains("id=\"new-entry-form\""));
    let bob = tag_with(&writer, "value=\"bob\"");
    assert!(bob.contains("name=\"assignees\""), "{bob}");
    assert!(bob.contains("form=\"new-entry-form\""), "{bob}");
    assert!(bob.contains("checked"), "{bob}");
    let bug = tag_with(&writer, "value=\"3\"");
    assert!(
        bug.contains("name=\"labels\"") && bug.contains("checked"),
        "{bug}"
    );
    assert!(tag_with(&writer, "value=\"1\"").contains("checked"));
    // A writer who cannot administer the repository picks assignees and a milestone, not labels.
    let writer_no_admin = render_for(true, false);
    assert!(writer_no_admin.contains("name=\"assignees\""));
    assert!(!writer_no_admin.contains("name=\"labels\""));
    let reader = render_for(false, false);
    assert!(!reader.contains("name=\"assignees\""));
    assert!(!reader.contains("name=\"labels\""));
    assert!(!reader.contains("name=\"milestone\""));
}

#[test]
fn markdown_editor_shows_its_formatting_toolbar_disabled_until_hydration() {
    let html = dioxus_ssr::render_element(rsx! {
        MarkdownEditor { id: "new-body", initial: "", rows: 4, required: false, initial_preview: false }
    });
    let toolbar = tag_with(&html, "class=\"collaboration-markdown-toolbar\"");
    assert!(!toolbar.contains("hidden"), "{toolbar}");
    for title in [
        "Heading",
        "Bold",
        "Italic",
        "Quote",
        "Code",
        "Link",
        "Bulleted list",
        "Numbered list",
        "Task list",
    ] {
        let tool = tag_with(&html, &format!("title=\"{title}\""));
        assert!(tool.contains("disabled"), "{tool}");
    }
    assert!(html.matches("type=\"button\"").count() >= 9);
}

#[test]
fn split_files_changed_pairs_lines_and_links_both_views() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: "@@ -1,3 +1,4 @@\n keep\n-old\n+new\n+more\n tail".into(),
            owners: Vec::new(),
        }],
        additions: 2,
        deletions: 1,
        viewed: None,
    };
    let repo = repository_at(Kind::Pull, "/7/diff");
    let split = dioxus_ssr::render_element(diff_page(
        &repo,
        &item(),
        &diff,
        &[],
        false,
        None,
        None,
        0,
        None,
        &Default::default(),
        true,
    ));
    assert!(split.contains("class=\"collaboration-split patch-lines\""));
    // The added line with no removed partner has an empty left side; the context lines mirror.
    assert_eq!(split.matches("collaboration-split-empty").count(), 1);
    assert_eq!(split.matches("collaboration-split-mirror").count(), 2);
    assert!(split.contains("collaboration-split-wide"));
    assert!(split.contains("Review changes"));
    assert!(tag_with(&split, "Review changes").contains("btn-primary"));
    assert!(split.contains("/7/diff?view=split"));
    // The author cannot approve their own pull request, and the menu says why.
    let mut own = item();
    own.comparison.as_mut().unwrap().can_vote = false;
    let mut author_repo = repository_at(Kind::Pull, "/7/diff");
    author_repo.viewer = "alice".into();
    let author = dioxus_ssr::render_element(diff_page(
        &author_repo,
        &own,
        &diff,
        &[],
        false,
        None,
        None,
        0,
        None,
        &Default::default(),
        true,
    ));
    assert!(author.contains("You opened this pull request"));

    let unified = dioxus_ssr::render_element(diff_page(
        &repo,
        &item(),
        &diff,
        &[],
        false,
        None,
        None,
        0,
        None,
        &Default::default(),
        false,
    ));
    // The inline stylesheet names the split classes too, so check the markup only.
    assert!(!unified.contains("class=\"collaboration-split"));
    assert!(unified.contains("/7/diff?view=split"));
}

#[test]
fn split_view_skips_git_header_lines_and_comments_return_to_it() {
    let diff = Diff {
        files: vec![DiffFile {
            filename: "src/lib.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 1,
        viewed: None,
    };
    let repo = repository_at(Kind::Pull, "/7/diff");
    let composer = LineComment {
        path: "src/lib.rs".into(),
        side: "RIGHT".into(),
        line: "1".into(),
        ..Default::default()
    };
    let split = dioxus_ssr::render_element(diff_page(
        &repo,
        &item(),
        &diff,
        &[],
        false,
        Some(&composer),
        None,
        0,
        None,
        &Default::default(),
        true,
    ));
    assert!(!split.contains("index 1111111"));
    assert!(!split.contains("--- a/src/lib.rs"));
    assert!(split.contains("-old"));
    assert!(split.contains("+new"));
    // The composer posts back to the split view and can be cancelled to it.
    assert!(split.contains("/comments?view=split"), "{split}");
    assert!(split.contains("Cancel"), "{split}");
}

#[test]
fn stack_card_marks_the_viewed_layer_and_strikes_merged_ones() {
    let mut item = item();
    item.stack = Some(StackMap {
        trunk: "main".into(),
        position: 2,
        restack: false,
        layers: vec![
            StackLayer {
                number: 6,
                title: "Base work".into(),
                state: "merged".into(),
                draft: false,
                current: false,
            },
            StackLayer {
                number: 7,
                title: "Next work".into(),
                state: "open".into(),
                draft: false,
                current: true,
            },
        ],
    });
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(detail(
            &repository(),
            Kind::Pull,
            &item,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ))
    });
    assert!(html.contains("collaboration-stack-merged"));
    assert!(html.contains("collaboration-stack-current"));
    // The top layer comes first and the trunk closes the list.
    assert!(html.find("#7 Next work").unwrap() < html.find("collaboration-stack-base").unwrap());
}

#[test]
fn stack_candidate_is_offered_as_one_banner_to_writers() {
    let mut item = item();
    item.stack_candidate = Some(6);
    let mut repo = repository();
    repo.can_write = true;
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(detail(
            &repo,
            Kind::Pull,
            &item,
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
        ))
    });
    let banner = tag_with(&html, "collaboration-stack-banner");
    assert!(
        banner.contains("action=\"/alice/repo/pulls/7/stack\""),
        "{banner}"
    );
    assert!(html.contains("based on #6"));
    assert!(html.contains(">Add to stack</button>"));
}

#[test]
fn new_pull_request_has_one_compare_action_and_a_hint_before_comparing() {
    let page = Page::New {
        repository: repository_at(Kind::Pull, "/new"),
        kind: Kind::Pull,
        draft: NewDraft::default(),
        branches: BranchChoices {
            repositories: vec![(1, "alice/repo".into())],
            base: vec!["main".into()],
            head: vec!["topic".into()],
        },
        choices: IssueChoices::default(),
        diff: None,
        error: None,
        preview: false,
        references: Default::default(),
    };
    let html = crate::webui::tests::in_scope(|| dioxus_ssr::render_element(render(page)));
    assert!(html.contains(">Compare</button>"));
    assert!(!html.contains("Load head branches"));
    assert!(!html.contains("Compare selected branches"));
    assert!(html.contains("collaboration-compare-hint"));
}

#[test]
fn label_picker_rows_show_a_colour_dot_and_a_check_for_applied_labels() {
    crate::webui::tests::in_scope(|| {
        let mut issue = item();
        issue.comparison = None;
        issue.labels = vec![Label {
            id: 11,
            name: "bug".into(),
            color: "#ff0000".into(),
            description: String::new(),
        }];
        let available = vec![
            issue.labels[0].clone(),
            Label {
                id: 12,
                name: "docs".into(),
                color: "#00ff00".into(),
                description: "Writing".into(),
            },
        ];
        let html = dioxus_ssr::render_element(super::detail(
            &repository_at(Kind::Issue, "/7"),
            Kind::Issue,
            &issue,
            &CommentPage::default(),
            &available,
            &DetailDraft::default(),
            None,
            &Default::default(),
            &[],
            &[],
            &[],
            &[],
        ));
        let picker = &html[html.find("Apply labels to this issue").unwrap()..];
        // Each row leads with a dot in its label's colour; only the applied label has a check.
        assert_eq!(
            picker.matches("class=\"collaboration-picker-dot\"").count(),
            2
        );
        assert!(html.contains(
            "collaboration-picker-dot\" style=\"border: 1px solid #ff0000; background: #ff0000"
        ));
        assert!(html.contains(
            "collaboration-picker-dot\" style=\"border: 1px solid #00ff00; background: #00ff00"
        ));
        assert_eq!(picker.matches(">✓</span>").count(), 1);
        assert!(picker.contains("Writing"));
    });
}

#[test]
fn assignee_picker_rows_show_avatars_and_a_check_and_offer_assign_yourself() {
    crate::webui::tests::in_scope(|| {
        let mut repo = repository_at(Kind::Pull, "/7");
        repo.viewer = "bob".into();
        let assignable = vec!["alice".to_string(), "bob".to_string()];
        let render = |assignees: Vec<String>| {
            let mut entry = item();
            entry.assignees = assignees;
            dioxus_ssr::render_element(super::detail(
                &repo,
                Kind::Pull,
                &entry,
                &CommentPage::default(),
                &[],
                &DetailDraft::default(),
                None,
                &Default::default(),
                &[],
                &[],
                &assignable,
                &[],
            ))
        };

        let html = render(vec!["alice".into()]);
        let picker = &html[html.find("Assign people to this issue").unwrap()..];
        // Bob is not assigned, so the menu opens with an Assign yourself button for him.
        let self_at = picker.find("Assign yourself").unwrap();
        assert!(self_at < picker.find("collaboration-picker-list").unwrap());
        assert!(picker.contains("name=\"username\" value=\"bob\""));
        // Every row shows the person's picture and name, and the assigned person has a check.
        assert!(picker.contains("src=\"/avatars/alice\""));
        assert!(picker.contains("src=\"/avatars/bob\""));
        assert_eq!(picker.matches(">✓</span>").count(), 1);

        // Once Bob is assigned too, there is nothing for the button to do.
        let html = render(vec!["alice".into(), "bob".into()]);
        assert!(!html.contains("Assign yourself"));
    });
}

#[test]
fn reviewer_picker_rows_show_avatars_and_checks_for_open_requests() {
    crate::webui::tests::in_scope(|| {
        let reviewers = [Reviewer {
            username: "bob".into(),
            status: ReviewerStatus::Requested,
            stale: false,
            pending: true,
            code_owner: false,
        }];
        let assignable = vec!["bob".to_string(), "carol".to_string()];
        let html = dioxus_ssr::render_element(super::detail(
            &repository_at(Kind::Pull, "/7"),
            Kind::Pull,
            &item(),
            &CommentPage::default(),
            &[],
            &DetailDraft::default(),
            None,
            &Default::default(),
            &reviewers,
            &assignable,
            &[],
            &[],
        ));
        let picker = &html[html.find("Request reviews from").unwrap()..];
        assert!(picker.contains("src=\"/avatars/bob\""));
        assert!(picker.contains("src=\"/avatars/carol\""));
        assert_eq!(picker.matches(">✓</span>").count(), 1);
    });
}
