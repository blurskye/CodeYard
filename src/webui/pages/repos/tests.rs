use super::blame::{blame, BlameHunk, BlamePage};
use super::blob::{blob, display_text, matching_entries, BlobPage, FolderListing};
use super::branches::{refs, RefRow};
use super::chrome::{active_tab, chrome, repo_engagement, repo_tabs, RepoChrome};
use super::commits::{commit_detail, commits, Commit, HistoryFilter};
use super::editor::{delete_file, editor, upload_page, DeleteFile, Editor, UploadForm};
use super::find::{find, FindPage};
use super::issues;
use super::page::{render, Page};
use super::pulls::{Diff, DiffFile, PullSummary};
use super::ref_path::file_url;
use super::settings::{rejected_form, settings, BranchRule, Collaborator, RuleDraft, Settings};
use super::tree::{tree, TreeEntry, TreePage, MAX_README_BYTES};
use crate::webui::components::code::{self, themed_highlight};
use crate::webui::components::compact_date;
use crate::webui::components::languages::LanguageShare;
use dioxus::prelude::*;

fn fixtures() -> Vec<Page> {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        description: "An actual description".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        can_admin: true,
        is_owner: true,
        current_path: "/alice/project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "abcdef1234567890".into(),
        message: "Initial commit".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: vec!["123456abcdef".into()],
    };
    let blob = BlobPage {
        reference: "feature/x".into(),
        path: "a #?.rs".into(),
        content: "<script>alert(1)</script>".into(),
        binary: false,
        size: 29,
        offset: 0,
        first_line: Some(1),
        next: Some(256),
        previous: None,
        truncated: true,
        can_blame: false,
        editable: true,
        deletable: true,
        code_view: false,
        references: Default::default(),
        folders: Vec::new(),
        latest_commit: None,
        branches: vec!["feature/x".into()],
        tags: vec!["v1/release".into()],
        permalink: "https://example.invalid/alice/project/blob/0123456789/a%20%23%3F.rs".into(),
    };
    let rows = vec![RefRow {
        name: "feature/x".into(),
        sha: commit.sha.clone(),
        message: Some("Release notes".into()),
        is_default: false,
        ahead_behind: None,
        updated: None,
        pull: None,
    }];
    let editor = Editor {
        branch: "feature/x".into(),
        path: "file.md".into(),
        content: "</textarea><script>alert(1)</script>".into(),
        message: "Save edited content".into(),
        expected_revision: "0123456789012345678901234567890123456789".into(),
        creating: false,
        error: Some("The branch changed.".into()),
        saved: false,
        preview: false,
        references: Default::default(),
        protected: false,
        commit_to_new_branch: false,
        new_branch: String::new(),
        description: String::new(),
    };
    vec![
        Page::Tree {
            repo: repo.clone(),
            page: TreePage {
                languages: Vec::new(),
                releases: None,
                reference: "feature/x".into(),
                path: "docs".into(),
                branches: vec!["feature/x".into()],
                tags: vec!["v1/release".into()],
                entries: vec![TreeEntry {
                    name: "ReadMe.md".into(),
                    path: "docs/ReadMe.md".into(),
                    kind: "file".into(),
                    size: Some(24),
                    last_commit: Some(commit.clone()),
                }],
                readme: Some("# README\nActual repository content".into()),
                latest_commit: Some(commit.clone()),
                folders: Vec::new(),
                permalink: "https://example.invalid/alice/project/tree/0123456789/docs".into(),
                empty: false,
                http_clone: "https://example.invalid/alice/project.git".into(),
                ssh_clone: "ssh://git@example.invalid/alice/project.git".into(),
                references: Default::default(),
            },
        },
        Page::Blob {
            repo: repo.clone(),
            page: blob.clone(),
        },
        Page::Blob {
            repo: repo.clone(),
            page: BlobPage {
                binary: true,
                content: String::new(),
                editable: false,
                ..blob
            },
        },
        Page::Find {
            repo: repo.clone(),
            page: FindPage {
                reference: "main".into(),
                paths: vec!["README.md".into(), "src/<lib>.rs".into()],
                truncated: true,
                query: "lib".into(),
            },
        },
        Page::Blame {
            repo: repo.clone(),
            page: BlamePage {
                reference: "feature/x".into(),
                path: "a #?.rs".into(),
                content: "<script>alert(1)</script>\nlet two = 2;\n".into(),
                size: 38,
                hunks: vec![
                    BlameHunk {
                        first_line: 1,
                        line_count: 1,
                        sha: commit.sha.clone(),
                        author: "Alice".into(),
                        author_date: commit.author_date.clone(),
                        summary: "Initial commit".into(),
                    },
                    BlameHunk {
                        first_line: 2,
                        line_count: 1,
                        sha: "fedcba0987654321".into(),
                        author: "<b>Bob</b>".into(),
                        author_date: "2026-10-03T00:00:00Z".into(),
                        summary: "Change two".into(),
                    },
                ],
                ..Default::default()
            },
        },
        Page::Commits {
            repo: repo.clone(),
            rows: vec![commit.clone()],
            reference: "feature/x".into(),
            path: String::new(),
            filter: Default::default(),
            page: 2,
            has_more: true,
            branches: vec!["feature/x".into()],
            tags: vec!["v1/release".into()],
        },
        Page::CommitDetail {
            repo: repo.clone(),
            commit: commit.clone(),
            diff: Diff {
                files: vec![DiffFile {
                    filename: "file.rs".into(),
                    status: "modified".into(),
                    additions: 1,
                    deletions: 1,
                    patch: "-old\n+<script>alert(1)</script>".into(),
                    owners: Vec::new(),
                }],
                additions: 1,
                deletions: 1,
                viewed: None,
            },
            branches: vec!["feature/x".into()],
            tags: vec!["v1/release".into()],
            split: false,
        },
        Page::Refs {
            repo: repo.clone(),
            rows: rows.clone(),
            tags: false,
            branches: rows.clone(),
            query: String::new(),
            tab: "all".into(),
            stale: 0,
        },
        Page::Refs {
            repo: repo.clone(),
            rows: rows.clone(),
            tags: true,
            branches: rows,
            query: String::new(),
            tab: String::new(),
            stale: 0,
        },
        Page::Editor {
            repo: repo.clone(),
            editor: editor.clone(),
        },
        Page::Editor {
            repo: repo.clone(),
            editor: Editor {
                preview: true,
                creating: true,
                ..editor
            },
        },
        Page::DeleteFile {
            repo: repo.clone(),
            file: DeleteFile {
                branch: "main".into(),
                path: "docs/old.md".into(),
                expected_revision: "abc".into(),
                message: "Delete docs/old.md".into(),
                error: Some("This branch is protected".into()),
                saved: false,
                protected: true,
                commit_to_new_branch: true,
                new_branch: "alice-patch-1".into(),
            },
        },
        Page::Upload {
            repo: repo.clone(),
            form: UploadForm {
                repository_id: repo.id,
                commit_to_new_branch: false,
                branch: "main".into(),
                directory: "docs".into(),
                expected_revision: "abc".into(),
                message: "</textarea><script>alert(1)</script>".into(),
                description: String::new(),
                error: Some("Each file can be at most 25 MiB.".into()),
                saved: false,
                protected: false,
                new_branch: String::new(),
            },
        },
        Page::Settings {
            repo: repo.clone(),
            settings: Settings {
                webhooks: None,
                deploy_keys: None,
                tab: "general".into(),
                branches: Vec::new(),
                default_branch: String::new(),
                collaborators: vec![Collaborator {
                    username: "bob".into(),
                    access: "write".into(),
                }],
                allow_merge_commit: true,
                allow_squash_merge: false,
                allow_rebase_merge: true,
                branch_rules: Vec::new(),
                rule_draft: None,
                error: None,
                saved: false,
                viewer: String::new(),
                force_push_suggestions: Vec::new(),
            },
        },
        Page::RejectedForm {
            repo,
            action: "create-tag".into(),
            message: "Check the source revision.".into(),
            values: std::collections::HashMap::from([
                ("name".into(), "v1/release".into()),
                (
                    "message".into(),
                    "</textarea><script>alert(1)</script>".into(),
                ),
            ]),
        },
        Page::Releases {
            repo: RepoChrome {
                owner: "alice".into(),
                name: "project".into(),
                ..Default::default()
            },
            page: crate::webui::pages::repos::releases::Page::Detail {
                release: crate::webui::pages::repos::releases::ReleaseItem {
                    id: 1,
                    tag: "v1.0".into(),
                    title: "Version </h1><script>alert(1)</script>".into(),
                    body: "Notes".into(),
                    author: "alice".into(),
                    target: "main".into(),
                    created_at: "2026-10-01T00:00:00Z".into(),
                    published_at: Some("2026-10-02T00:00:00Z".into()),
                    draft: false,
                    prerelease: false,
                    latest: true,
                },
                assets: Vec::new(),
                can_write: false,
            },
        },
    ]
}

#[test]
fn every_page_variant_roundtrips_only_presentation_data() {
    for page in fixtures() {
        let json = serde_json::to_string(&page).unwrap();
        let decoded: Page = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, page);
        let data: serde_json::Value = serde_json::from_str(&json).unwrap();
        fn assert_safe_fields(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (key, value) in fields {
                        assert!(
                            ![
                                "password",
                                "password_hash",
                                "disk_path",
                                "owner_id",
                                "redis",
                                "pool",
                                "dangerous_inner_html"
                            ]
                            .contains(&key.as_str()),
                            "unexpected operational field {key}"
                        );
                        assert_safe_fields(value);
                    }
                }
                serde_json::Value::Array(values) => values.iter().for_each(assert_safe_fields),
                _ => {}
            }
        }
        assert_safe_fields(&data);
    }
}

#[test]
fn typed_dispatch_matches_pure_views_and_escapes_all_variants() {
    crate::webui::tests::in_scope(|| {
        for page in fixtures() {
            let expected = match &page {
                Page::Tree { repo, page } => tree(repo, page),
                Page::Blob { repo, page } => blob(repo, page),
                Page::Blame { repo, page } => blame(repo, page),
                Page::Find { repo, page } => find(repo, page),
                Page::Commits {
                    repo,
                    rows,
                    reference,
                    path,
                    filter,
                    page,
                    has_more,
                    branches,
                    tags,
                } => commits(
                    repo, rows, reference, path, filter, *page, *has_more, branches, tags,
                ),
                Page::CommitDetail {
                    repo,
                    commit,
                    diff,
                    branches,
                    tags,
                    split,
                } => commit_detail(repo, commit, diff, branches, tags, *split),
                Page::Refs {
                    repo,
                    rows,
                    tags,
                    branches,
                    query,
                    tab,
                    stale,
                } => refs(repo, rows, *tags, branches, query, tab, *stale),
                Page::Editor {
                    repo,
                    editor: state,
                } => editor(repo, state),
                Page::DeleteFile { repo, file } => delete_file(repo, file),
                Page::Upload { repo, form } => upload_page(repo, form),
                Page::Settings {
                    repo,
                    settings: state,
                } => settings(repo, state),
                Page::RejectedForm {
                    repo,
                    action,
                    message,
                    values,
                } => rejected_form(repo, action, message, values),
                Page::Releases { repo, page } => {
                    crate::webui::pages::repos::releases::render(repo, page.clone())
                }
            };
            let expected = dioxus_ssr::render_element(expected);
            let decoded: Page =
                serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
            let actual = dioxus_ssr::render_element(render(decoded));
            assert_eq!(actual, expected);
            assert!(!actual.contains("<script"));
            assert!(!actual.contains("onclick="));
            assert_eq!(
                actual.matches("<form ").count(),
                actual.matches("name=\"csrf_token\"").count()
                    + actual.matches("method=\"get\"").count()
            );
        }
    });
}

#[test]
fn copy_controls_and_navigation_keep_native_protocol_fallbacks() {
    let pages = fixtures();
    let tree_html = dioxus_ssr::render_element(render(pages[0].clone()));
    assert!(tree_html.contains("Copy HTTPS"));
    assert!(tree_html.contains("Copy SSH"));
    assert!(!tree_html.contains("href=\"/alice/project/tree/refs/heads/feature/x/docs/ReadMe.md\""));
    assert!(tree_html.contains("href=\"/alice/project/blob/refs/heads/feature/x/docs/ReadMe.md\""));
    assert!(tree_html.contains("/alice/project/archive/feature%2Fx.zip"));
    let blob_html = dioxus_ssr::render_element(render(pages[1].clone()));
    assert!(blob_html.contains("Copy preview"));
    assert!(blob_html.contains("href=\"/alice/project/raw/refs/heads/feature/x/a%20%23%3F.rs\""));
    assert!(blob_html.contains("href=\"/alice/project/edit/refs/heads/feature/x/a%20%23%3F.rs\""));
}

#[test]
fn code_view_numbers_whole_files_but_not_mid_file_previews() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let mut page = BlobPage {
        reference: "main".into(),
        path: "src/main.rs".into(),
        content: "fn main() {\n    run();\n}\n".into(),
        size: 24,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(html.contains(
        "<pre class=\"repo-line-numbers\"><a href=\"#L1\">1</a><a href=\"#L2\">2</a><a href=\"#L3\">3</a></pre>"
    ));
    assert!(html.contains("<span class=\"repo-code-line\" id=\"L2\">"));
    page.offset = 4096;
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(!html.contains("<pre class=\"repo-line-numbers\""));
}

#[test]
fn parent_refresh_preserves_editor_revision_and_settings_drafts() {
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Harness {
        initial: Page,
        page: Rc<RefCell<Option<Signal<Page>>>>,
    }
    fn root(props: Harness) -> Element {
        let page = use_signal(|| props.initial.clone());
        *props.page.borrow_mut() = Some(page);
        render(page())
    }
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    let state = Editor {
        branch: "main".into(),
        path: "file.md".into(),
        content: "retained draft".into(),
        message: "retained message".into(),
        expected_revision: "old-revision".into(),
        ..Default::default()
    };
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(
        root,
        Harness {
            initial: Page::Editor {
                repo: repo.clone(),
                editor: state.clone(),
            },
            page: control.clone(),
        },
    );
    dom.rebuild_in_place();
    let mut page = control.borrow().unwrap();
    let changed = Editor {
        content: "new server content".into(),
        message: "new server message".into(),
        expected_revision: "new-revision".into(),
        ..state.clone()
    };
    dom.in_runtime(|| {
        page.set(Page::Editor {
            repo: repo.clone(),
            editor: changed.clone(),
        })
    });
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("retained draft"));
    assert!(html.contains("retained message"));
    assert!(html.contains("old-revision"));
    assert!(!html.contains("new-revision"));
    assert!(!html.contains("new server content"));
    // Deliberate route identity changes initialize a fresh, matching snapshot.
    dom.in_runtime(|| {
        page.set(Page::Editor {
            repo: repo.clone(),
            editor: Editor {
                path: "other.md".into(),
                ..changed
            },
        })
    });
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("new-revision"));
    assert!(html.contains("new server content"));
    let repo = RepoChrome {
        description: "settings draft".into(),
        ..repo
    };
    dom.in_runtime(|| {
        page.set(Page::Settings {
            repo: repo.clone(),
            settings: Settings::default(),
        })
    });
    dom.render_immediate_to_vec();
    dom.in_runtime(|| {
        page.set(Page::Settings {
            repo: RepoChrome {
                description: "new stored description".into(),
                is_private: true,
                ..repo.clone()
            },
            settings: Settings::default(),
        })
    });
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("settings draft"));
    assert!(!html.contains("new stored description</textarea>"));
}

#[test]
fn prerendered_repository_drafts_have_opaque_text_and_original_revision() {
    crate::webui::tests::in_scope(|| {
        let source = "\n</textarea><script>bad</script>&";
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            description: source.into(),
            csrf_token: "csrf-fixture".into(),
            can_write: true,
            ..Default::default()
        };
        let state = Editor {
            branch: "main".into(),
            path: "file.md".into(),
            content: source.into(),
            message: "kept message".into(),
            expected_revision: "revision-snapshot".into(),
            preview: true,
            ..Default::default()
        };
        let fields = std::collections::HashMap::from([("message".into(), source.into())]);
        // The editor has a second textarea, the optional extended description.
        for (element, textareas) in [
            (editor(&repo, &state), 2),
            (settings(&repo, &Settings::default()), 1),
            (refs(&repo, &[], true, &[], "", "", 0), 1),
            (
                rejected_form(&repo, "create-tag", "Review this draft", &fields),
                1,
            ),
        ] {
            let mut renderer = dioxus_ssr::Renderer::new();
            renderer.pre_render = true;
            let html = renderer.render_element(element);
            let mut count = 0;
            for textarea in html.split("<textarea").skip(1) {
                let text = textarea
                    .split_once('>')
                    .unwrap()
                    .1
                    .split_once("</textarea>")
                    .unwrap()
                    .0;
                assert!(text.starts_with('\n'));
                assert!(!text.contains("<!--"));
                assert!(!text.contains("<script>"));
                if !text.trim().is_empty() {
                    assert_eq!(
                        text,
                        "\n\n&lt;/textarea&gt;&lt;script&gt;bad&lt;/script&gt;&amp;"
                    );
                }
                count += 1;
            }
            assert_eq!(count, textareas);
            assert!(html.contains("name=\"csrf_token\""));
            if html.contains("name=\"expected_revision\"") {
                assert!(html.contains("value=\"revision-snapshot\""));
                assert!(html.contains("value=\"kept message\""));
                assert!(html.contains("Preview"));
            }
        }
    });
}

#[test]
fn tabs_respect_segment_boundaries() {
    assert!(active_tab("/a/r", "/a/r/tree/feature/x", ""));
    assert!(active_tab("/a/r", "/a/r/commits", "/commits"));
    assert!(!active_tab("/a/r", "/a/repo", ""));
    assert!(!active_tab("/a/r", "/a/r/settings-extra", "/settings"));
    assert!(active_tab("/a/r", "/a/r/milestones", "/issues"));
    assert!(active_tab("/a/r", "/a/r/milestones/1", "/issues"));
    assert!(active_tab("/a/r", "/a/r/labels", "/issues"));
    assert!(!active_tab("/a/r", "/a/r/labels", "/pulls"));
    assert!(!active_tab("/a/r", "/a/r/milestones/1", "/pulls"));
    assert!(!active_tab("/a/r", "/a/r/milestonesx", "/issues"));
    for family in ["/issues", "/pulls"] {
        for tail in ["", "/new", "/7", "/7/edit", "/7/comments", "/7/diff"] {
            let path = format!("/a/r{family}{tail}");
            assert!(active_tab("/a/r", &path, family));
            assert!(!active_tab("/a/r", &path, ""));
            assert!(!active_tab("/a/r", &path, "/actions"));
        }
        assert!(!active_tab("/a/r", &format!("/a/r{family}-extra"), family));
        assert!(!active_tab("/a/r", &format!("/a/repo{family}"), family));
    }
}
#[test]
fn rendered_editor_escapes_text_and_has_native_csrf_forms() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        ..Default::default()
    };
    let editor = Editor {
        content: "</textarea><script>alert(1)</script>".into(),
        path: "a\" onfocus=\"x".into(),
        branch: "feature/x".into(),
        expected_revision: "abc".into(),
        creating: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    assert!(html.contains("method=\"post\""));
    assert!(html.contains("name=\"csrf_token\""));
    assert!(html.contains("csrf-fixture"));
    assert!(html.contains("name=\"expected_revision\""));
    assert!(html.contains("&lt;/textarea&gt;"));
    assert!(!html.contains("<script"));
    assert!(!html.contains(" onfocus=\"x"));
    assert!(!html.contains("onclick="));
    assert_eq!(
        html.matches("<form ").count(),
        html.matches("name=\"csrf_token\"").count()
    );
    assert!(html.contains("repo-code-editor"));
    assert!(html.contains("repo-commit-box"));
    assert!(html.contains("Cancel"));
    // A new file keeps its Preview button, because its path is typed in this form.
    assert!(html.contains("formnovalidate"));
}

#[test]
fn editor_offers_preview_only_for_markdown_files_and_new_files() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        ..Default::default()
    };
    let render = |path: &str, creating: bool| {
        let editor = Editor {
            path: path.into(),
            branch: "main".into(),
            expected_revision: "abc".into(),
            creating,
            ..Default::default()
        };
        dioxus_ssr::render_element(super::editor::editor(&repo, &editor))
    };
    assert!(!render("src/main.rs", false).contains(">Preview</button>"));
    assert!(render("README.md", false).contains(">Preview</button>"));
    assert!(render("docs/GUIDE.Markdown", false).contains(">Preview</button>"));
    assert!(render("docs/page.mdx", false).contains(">Preview</button>"));
    assert!(render("src/main.rs", true).contains(">Preview</button>"));
}

#[test]
fn editor_names_a_new_file_and_marks_large_text_as_plain() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let render = |editor: Editor| dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    let creating = Editor {
        path: "src/new.rs".into(),
        branch: "main".into(),
        expected_revision: "abc".into(),
        creating: true,
        ..Default::default()
    };
    let html = render(creating);
    assert!(html.contains("<strong>New file</strong>"), "{html}");
    assert!(!html.contains("<strong>Edit</strong>"), "{html}");
    assert!(!html.contains("Syntax highlighting is off"), "{html}");

    let large = Editor {
        path: "big.txt".into(),
        branch: "main".into(),
        expected_revision: "abc".into(),
        // Just past the 20 KiB highlight limit.
        content: "a".repeat(20 * 1024 + 1),
        ..Default::default()
    };
    let html = render(large);
    assert!(html.contains("<strong>Edit</strong>"), "{html}");
    assert!(
        html.contains("Syntax highlighting is off for large files."),
        "{html}"
    );
}

#[test]
fn preview_card_sits_above_the_commit_box() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let editor = Editor {
        path: "README.md".into(),
        branch: "main".into(),
        expected_revision: "abc".into(),
        content: "# Title\n".into(),
        preview: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    let preview = html.find("id=\"preview\"").expect("preview card");
    let commit = html.find("class=\"repo-commit-box").expect("commit box");
    let editor_card = html
        .find("class=\"repo-code-editor panel\"")
        .expect("editor card");
    assert!(editor_card < preview && preview < commit, "{html}");
    assert!(
        html.contains("formaction=\"") && html.contains("#preview\""),
        "{html}"
    );
}

#[test]
fn editor_numbers_every_line_of_the_text() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let editor = Editor {
        branch: "main".into(),
        path: "a.txt".into(),
        content: "one\ntwo\nthree".into(),
        expected_revision: "abc".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    assert!(html.contains("repo-code-editor-gutter"));
    assert!(html.contains(">1\n2\n3</pre>"), "{html}");
    assert!(html.contains("3 lines"));
    // A short file still gets a field tall enough to work in.
    assert!(html.contains("rows=10"), "{html}");
}

#[test]
fn editor_renders_the_plain_field_before_hydration() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let editor = Editor {
        branch: "main".into(),
        path: "src/main.rs".into(),
        content: "fn main() {}\n".into(),
        expected_revision: "abc".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    assert!(html.contains("id=\"repository-file-content\""), "{html}");
    assert!(html.contains("repo-editor-stack"), "{html}");
    // The highlight is drawn only after hydration, so the server-rendered field stays plain.
    assert!(!html.contains("repo-editor-highlight"), "{html}");
}

#[test]
fn editor_preview_renders_like_the_saved_markdown_file() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let source = "a\nb\n\n[guide](guide.md#top)\n\ntext[^n]\n\n[^n]: Footnote text.";
    let editor = Editor {
        branch: "main".into(),
        path: "docs/file.md".into(),
        content: source.into(),
        expected_revision: "abc".into(),
        preview: true,
        ..Default::default()
    };
    let page = BlobPage {
        reference: "main".into(),
        path: "docs/file.md".into(),
        content: source.into(),
        ..Default::default()
    };
    let rendered = |element| {
        let html = dioxus_ssr::render_element(element);
        let (_, body) = html
            .split_once("<div class=\"markdown-body leading-relaxed text-foreground\">")
            .expect("rendered Markdown");
        body.split_once("Footnote text.</p></div>")
            .expect("footnote definition")
            .0
            .to_owned()
    };
    let preview = rendered(super::editor::editor(&repo, &editor));
    assert_eq!(preview, rendered(super::blob::blob(&repo, &page)));
    assert!(preview.contains(">a b</p>"));
    assert!(preview.contains("<sup class=\"footnote-reference\">"));
    assert!(preview
        .contains("href=\"/alice/project/blob/refs/heads/main/docs/guide.md#user-content-top\""));
}
#[test]
fn rendered_settings_and_ref_deletion_require_confirmation() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        is_owner: true,
        can_manage_collaborators: true,
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    let settings_html = dioxus_ssr::render_element(settings(&repo, &Settings::default()));
    assert!(settings_html.contains("/_forms/settings"));
    assert!(settings_html.contains("name=\"confirm\""));
    assert!(!settings_html.contains("/_forms/transfer"));
    assert!(!settings_html.contains("/_forms/archive"));
    assert!(settings_html.contains("repo-settings-nav"));
    let danger_html = settings_tab_html(&repo, "danger");
    assert!(danger_html.contains("name=\"confirmation\""));
    assert!(danger_html.contains("repo-danger-zone"));
    assert!(danger_html.contains("/_forms/transfer"));
    assert!(danger_html.contains("name=\"new_owner\""));
    let collaborators_html = settings_tab_html(&repo, "collaborators");
    assert!(collaborators_html.contains("No collaborators have been added."));
    let merging_html = settings_tab_html(&repo, "merging");
    assert!(merging_html.contains("/_forms/merge-methods"));
    assert!(merging_html.contains("name=\"allow_merge_commit\""));
    let rows = [RefRow {
        name: "feature/x".into(),
        sha: "abc".into(),
        message: None,
        is_default: false,
        ahead_behind: None,
        updated: None,
        pull: None,
    }];
    let refs_html = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows, "", "", 0));
    assert!(refs_html.contains("/_forms/delete-branch"));
    assert!(refs_html.contains("name=\"confirm\""));
    assert!(refs_html.contains("<details"));
    assert!(!refs_html.contains("onclick="));
    assert!(refs_html.contains("repo-ref-list-panel"));
    assert!(refs_html.contains("repo-ref-create"));
}

#[test]
fn settings_destructive_actions_ask_in_centred_dialogs() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        is_owner: true,
        can_manage_collaborators: true,
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    let collaborators = dioxus_ssr::render_element(settings(
        &repo,
        &Settings {
            tab: "collaborators".into(),
            branches: Vec::new(),
            default_branch: String::new(),
            collaborators: vec![Collaborator {
                username: "bob".into(),
                access: "write".into(),
            }],
            ..Settings::default()
        },
    ));
    assert!(collaborators.contains("aria-label=\"Remove bob from this repository\""));
    assert!(collaborators.contains("action=\"/alice/project/_forms/remove-collaborator\""));
    assert!(collaborators.contains("href=\"/alice/project/settings?tab=collaborators\""));
    assert!(!collaborators.contains("class=\"repo-inline-confirm\""));

    let danger = settings_tab_html(&repo, "danger");
    for (title, action) in [
        ("Transfer this repository", "/alice/project/_forms/transfer"),
        ("Delete this repository", "/alice/project/_forms/delete"),
    ] {
        assert!(
            danger.contains(&format!("aria-label=\"{title}\"")),
            "{danger}"
        );
        assert!(danger.contains(&format!("action=\"{action}\"")), "{danger}");
    }
    assert!(danger.contains("href=\"/alice/project/settings?tab=danger\""));
    assert!(!danger.contains("class=\"repo-inline-confirm\""));

    let branches = dioxus_ssr::render_element(settings(
        &repo,
        &Settings {
            tab: "branches".into(),
            branches: Vec::new(),
            default_branch: String::new(),
            branch_rules: vec![BranchRule {
                pattern: "release/**".into(),
                ..BranchRule::default()
            }],
            ..Settings::default()
        },
    ));
    assert!(branches.contains("aria-label=\"Delete rule release/**\""));
    assert!(branches.contains("action=\"/alice/project/_forms/protection-delete\""));
    assert!(branches.contains("href=\"/alice/project/settings?tab=branches\""));
    assert!(!branches.contains("class=\"repo-inline-confirm\""));
}

/// The settings page as it renders on one section.
fn settings_tab_html(repo: &RepoChrome, tab: &str) -> String {
    dioxus_ssr::render_element(settings(
        repo,
        &Settings {
            tab: tab.into(),
            branches: Vec::new(),
            default_branch: String::new(),
            ..Settings::default()
        },
    ))
}

/// The opening tag of the settings nav link to `tab`.
fn settings_nav_link<'a>(html: &'a str, tab: &str) -> &'a str {
    let href = format!("href=\"/alice/project/settings?tab={tab}\"");
    let at = html
        .find(&href)
        .unwrap_or_else(|| panic!("the {tab} link: {html}"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

#[test]
fn each_settings_tab_renders_only_its_section_and_marks_itself_current() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        is_owner: true,
        can_manage_collaborators: true,
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    // Each section has a form or panel that no other section renders.
    let sections = [
        ("general", "_forms/settings\""),
        ("merging", "_forms/merge-methods\""),
        ("branches", "_forms/protection-save\""),
        ("collaborators", "_forms/collaborator\""),
        ("danger", "repo-danger-zone"),
    ];
    for (tab, _) in sections {
        let html = settings_tab_html(&repo, tab);
        for (name, marker) in sections {
            assert_eq!(
                html.contains(marker),
                name == tab,
                "{tab} page, {name} section: {html}"
            );
        }
        for (name, _) in sections {
            let link = settings_nav_link(&html, name);
            assert_eq!(
                link.contains("aria-current=\"page\""),
                name == tab,
                "{tab} page, {name} link: {link}"
            );
        }
    }
}

#[test]
fn settings_count_pills_follow_their_headings_and_the_label_swatch_has_the_color() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        is_owner: true,
        can_manage_collaborators: true,
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    // The count sits in the heading's own row, straight after the heading text.
    for (tab, heading) in [
        ("collaborators", "Collaborators"),
        ("branches", "Branch protection"),
    ] {
        let html = settings_tab_html(&repo, tab);
        let row = html
            .split_once("class=\"title-with-count\"")
            .unwrap_or_else(|| panic!("{tab} heading row: {html}"))
            .1
            .split_once("</div>")
            .unwrap()
            .0;
        assert!(row.contains(&format!("<h3>{heading}</h3>")), "{tab}: {row}");
        assert!(row.contains("count-badge"), "{tab}: {row}");
        assert!(!html.contains("repo-count-badge"), "{tab}: {html}");
    }
}

#[test]
fn label_colours_reach_a_style_only_when_they_are_valid_hex() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        is_owner: true,
        can_manage_collaborators: true,
        can_write: true,
        can_admin: true,
        ..Default::default()
    };
    let repository = issues::Repository {
        owner: "alice".into(),
        name: "project".into(),
        signed_in: true,
        can_write: true,
        can_admin: true,
        csrf: "csrf-fixture".into(),
        viewer: "alice".into(),
        chrome: repo.clone(),
    };
    // The labels page reads runtime state, so it renders inside the test runtime.
    let page = |color: &str| {
        crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(issues::render(issues::Page::Labels {
                repository: repository.clone(),
                items: vec![issues::Label {
                    id: 4,
                    name: "bug".into(),
                    color: color.into(),
                    description: String::new(),
                }],
                usage: Default::default(),
                query: String::new(),
                sort: String::new(),
                page_number: 1,
                has_next: false,
            }))
        })
    };
    // A valid colour reaches the chip and the edit swatch.
    let html = page("d73a4a");
    assert!(html.contains("background-color: #d73a4a"), "{html}");
    assert!(
        html.contains("background: #d73a4a; color: #ffffff"),
        "{html}"
    );
    // A stored colour that is not six hex digits never reaches a style attribute.
    let html = page("url(http://x.test)");
    assert!(!html.contains("background-color: url"), "{html}");
    assert!(!html.contains("background: url"), "{html}");
    assert!(html.contains("background-color: var(--muted)"), "{html}");
}

#[test]
fn native_controls_are_styled_once_in_the_shared_stylesheet() {
    let css = include_str!("../../components/native.css");
    assert!(css.contains("select:not([multiple]):not([size])"));
    assert!(css.contains("input[type=\"file\"]::file-selector-button"));
    // The repository chrome no longer carries its own copy of the select rule or the upload input.
    assert!(!include_str!("style.css").contains("select:not([multiple]):not([size])"));
    assert!(!css.contains("repo-upload-input"));
}

#[test]
fn unknown_settings_tab_shows_general() {
    let mut repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_manage_collaborators: true,
        can_admin: true,
        ..Default::default()
    };
    for tab in ["", "keys", "Merging"] {
        let html = settings_tab_html(&repo, tab);
        assert!(html.contains("_forms/settings\""), "{tab}: {html}");
        assert!(
            settings_nav_link(&html, "general").contains("aria-current=\"page\""),
            "{tab}: {html}"
        );
    }
    // A repository whose members are managed elsewhere has no Collaborators tab to show.
    repo.can_manage_collaborators = false;
    let html = settings_tab_html(&repo, "collaborators");
    assert!(html.contains("_forms/settings\""), "{html}");
    assert!(!html.contains("tab=collaborators"), "{html}");
}

#[test]
fn refused_settings_form_retry_page_cancels_back_to_its_section() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(rejected_form(
        &repo,
        "delete",
        "Type the complete owner/repository name to confirm permanent deletion.",
        &std::collections::HashMap::new(),
    ));
    assert!(
        html.contains("href=\"/alice/project/settings?tab=danger\""),
        "{html}"
    );
}

#[test]
fn branch_rows_compare_with_the_default_branch_and_tags_do_not() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        ..Default::default()
    };
    let row = |name: &str, is_default: bool| RefRow {
        name: name.into(),
        sha: "abc".into(),
        message: None,
        is_default,
        ahead_behind: None,
        updated: None,
        pull: None,
    };
    let branches = [row("main", true), row("feature/x", false)];
    let html = dioxus_ssr::render_element(refs(&repo, &branches, false, &branches, "", "", 0));
    assert!(html.contains("href=\"/alice/project/compare/main...feature/x\""));
    assert!(!html.contains("compare/main...main"));
    let tags =
        dioxus_ssr::render_element(refs(&repo, &[row("v1", false)], true, &branches, "", "", 0));
    assert!(!tags.contains("/compare/"));
}

#[test]
fn releases_are_reachable_from_the_tags_page_and_the_about_column() {
    use crate::webui::pages::repos::releases::{sidebar, ReleasesSummary};
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        ..Default::default()
    };
    let row = RefRow {
        name: "v1".into(),
        sha: "abc".into(),
        message: None,
        is_default: false,
        ahead_behind: None,
        updated: None,
        pull: None,
    };
    let tags = dioxus_ssr::render_element(refs(&repo, &[row], true, &[], "", "", 0));
    assert!(tags.contains("href=\"/alice/project/releases\""), "{tags}");
    let empty = dioxus_ssr::render_element(sidebar(
        &repo,
        &ReleasesSummary {
            count: 0,
            latest_tag: None,
        },
    ));
    assert!(
        empty.contains("href=\"/alice/project/releases\""),
        "{empty}"
    );
    assert!(
        empty.contains("href=\"/alice/project/releases/new\""),
        "{empty}"
    );
}
#[test]
fn ref_counts_sit_beside_headings_and_deletes_are_outlined() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_write: true,
        ..Default::default()
    };
    let row = |name: &str| RefRow {
        name: name.into(),
        sha: "abc".into(),
        message: None,
        is_default: false,
        ahead_behind: None,
        updated: None,
        pull: None,
    };
    let branches = dioxus_ssr::render_element(refs(
        &repo,
        &[row("main"), row("dev")],
        false,
        &[row("main")],
        "",
        "",
        0,
    ));
    assert!(branches.contains("2 branches"));
    assert!(branches.contains("title-with-count"));
    assert!(!branches.contains(" total<"));
    assert!(branches.contains("repo-icon-danger"));
    assert!(!branches.contains("btn-destructive"));
    let tags =
        dioxus_ssr::render_element(refs(&repo, &[row("v1")], true, &[row("main")], "", "", 0));
    assert!(tags.contains("1 tag<"));
    // The danger zone keeps the filled button.
    let settings_html = settings_tab_html(&repo, "danger");
    assert!(settings_html.contains("btn btn-destructive"));
}

#[test]
fn commit_history_groups_by_day_and_pages_with_newer_and_older() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = |sha: &str, message: &str, date: &str| Commit {
        sha: sha.repeat(40),
        message: message.into(),
        author: "Ada".into(),
        author_date: format!("{date}T00:00:00Z"),
        committer: "Ada".into(),
        committer_date: format!("{date}T00:00:00Z"),
        parents: Vec::new(),
    };
    let rows = vec![
        commit("a", "Newer change", "2026-10-02"),
        commit("b", "Older change", "2026-10-01"),
    ];
    let filter = HistoryFilter {
        author: "ada".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(commits(
        &repo,
        &rows,
        "main",
        "",
        &filter,
        2,
        true,
        &[],
        &[],
    ));
    assert!(html.contains("Commits on Oct 2, 2026"), "{html}");
    assert!(html.contains("Commits on Oct 1, 2026"), "{html}");
    // The middle page offers both directions, and the pager keeps the author filter.
    assert!(
        html.contains(">Newer<") && html.contains(">Older<"),
        "{html}"
    );
    assert!(
        html.contains("author=ada") && html.contains("page=1"),
        "{html}"
    );
    assert!(!html.contains("Page 2"), "{html}");
    assert!(html.contains("name=\"author\""), "{html}");
    // The Author menu lists the people on the page, and the Date menu offers ranges that keep the author.
    assert!(html.contains("Filter by author"), "{html}");
    assert!(html.contains("/avatars/Ada"), "{html}");
    assert!(html.contains("since=week"), "{html}");
    assert!(html.contains("Since yesterday"), "{html}");
}

#[test]
fn commit_filter_buttons_show_their_value_and_clear_with_one_x() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let filter = HistoryFilter {
        author: "alice".into(),
        since: "2026-10-01".into(),
        until: "2026-10-17".into(),
    };
    let html =
        dioxus_ssr::render_element(commits(&repo, &[], "main", "", &filter, 1, false, &[], &[]));
    assert!(html.contains(">Author: alice<"), "{html}");
    assert!(html.contains(">Date: 2026-10-01 to 2026-10-17<"), "{html}");
    assert!(html.contains("aria-label=\"Clear filters\""), "{html}");
    let unfiltered = dioxus_ssr::render_element(commits(
        &repo,
        &[],
        "main",
        "",
        &HistoryFilter::default(),
        1,
        false,
        &[],
        &[],
    ));
    assert!(unfiltered.contains(">Author<"), "{unfiltered}");
    assert!(
        !unfiltered.contains("aria-label=\"Clear filters\""),
        "{unfiltered}"
    );
    // The panels open from their button's left edge, and on a phone from the filter row's, so they
    // stay inside the window.
    let css = include_str!("style.css");
    assert!(css.contains(".native-menu-panel.repo-filter-panel { left: 0; right: auto; }"));
    assert!(css.contains(".repo-commit-filters { position: relative; }"));
}

#[test]
fn new_label_fields_share_one_grid_with_a_fixed_label_column() {
    let css = include_str!("style.css");
    assert!(css.contains(
        ".collaboration-label-fields { display: grid; grid-template-columns: 112px minmax(0, 1fr);"
    ));
    assert!(css.contains(".collaboration-label-fields > label { display: contents;"));
}

#[test]
fn commit_summary_shows_parents_the_full_id_and_refs_at_the_commit() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "c".repeat(40),
        message: "Add notes".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: vec!["p".repeat(40)],
    };
    let html = dioxus_ssr::render_element(commit_detail(
        &repo,
        &commit,
        &Diff::default(),
        &["main".into()],
        &["v1".into()],
        false,
    ));
    assert!(html.contains("repo-commit-parents"), "{html}");
    assert!(html.contains("ppppppp</a>"), "{html}");
    assert!(
        html.contains(&format!("{}</code>", "c".repeat(40))),
        "{html}"
    );
    assert!(html.contains("aria-label=\"Copy SHA\""), "{html}");
    assert!(
        html.contains("main</a>") && html.contains("v1</a>"),
        "{html}"
    );
}

#[test]
fn commit_page_prints_the_subject_once_and_the_rest_of_the_message_below_it() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "c".repeat(40),
        message: "Fix bug\n\nDetails here".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: vec![],
    };
    let render = |commit: &Commit| {
        dioxus_ssr::render_element(commit_detail(
            &repo,
            commit,
            &Diff::default(),
            &[],
            &[],
            false,
        ))
    };
    let html = render(&commit);
    assert_eq!(html.matches("Fix bug").count(), 1, "{html}");
    assert!(html.contains("<pre>Details here</pre>"), "{html}");
    let one_line = Commit {
        message: "Fix bug".into(),
        ..commit
    };
    assert!(!render(&one_line).contains("<pre>"));
}

#[test]
fn branch_list_searches_by_name_and_shows_divergence_and_delete_icons() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        ..Default::default()
    };
    let row = |name: &str, is_default: bool, ahead_behind: Option<(usize, usize)>| RefRow {
        name: name.into(),
        sha: "abc1234".into(),
        message: None,
        is_default,
        ahead_behind,
        updated: Some("2026-10-02T00:00:00Z".into()),
        pull: None,
    };
    let rows = [
        row("main", true, None),
        row("feature/login", false, Some((2, 1))),
        row("docs", false, Some((0, 0))),
    ];
    let html = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows, "", "", 0));
    assert!(html.contains("2 ahead, 1 behind main"), "{html}");
    assert!(html.contains("aria-label=\"Delete branch\""), "{html}");
    assert!(html.contains("name=\"q\""), "{html}");
    // A search keeps the rows whose name contains it, in any case.
    let found = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows, "LOGIN", "", 0));
    assert!(found.contains("feature/login</a>"), "{found}");
    assert!(!found.contains("docs</a>"), "{found}");
    let none = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows, "zzz", "", 0));
    assert!(none.contains("No matches"), "{none}");
}

#[test]
fn branch_row_links_to_the_open_pull_request_from_it() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let row = |name: &str, pull: Option<PullSummary>| RefRow {
        name: name.into(),
        sha: "abc1234".into(),
        message: None,
        is_default: false,
        ahead_behind: None,
        updated: None,
        pull,
    };
    let rows = [
        row(
            "feature/login",
            Some(PullSummary {
                number: 2,
                title: "Add login".into(),
            }),
        ),
        row("docs", None),
    ];
    let html = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows, "", "", 0));
    // The branch links to its open pull request, with the title as the tooltip.
    assert!(html.contains("href=\"/alice/project/pulls/2\""), "{html}");
    assert!(html.contains("title=\"Add login\""), "{html}");
    assert!(html.contains("#2</a>"), "{html}");
    // The other branch has no pull request, so there is one link only.
    assert_eq!(html.matches("/alice/project/pulls/").count(), 1, "{html}");
}

#[test]
fn tag_rows_show_their_date_in_a_column() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let tags = [RefRow {
        name: "v1".into(),
        sha: "abc1234".into(),
        message: None,
        is_default: false,
        ahead_behind: None,
        updated: Some("2026-10-02T00:00:00Z".into()),
        pull: None,
    }];
    let html = dioxus_ssr::render_element(refs(&repo, &tags, true, &[], "", "", 0));
    assert!(html.contains("class=\"repo-ref-date\""), "{html}");
    assert!(html.contains("datetime=\"2026-10-02T00:00:00Z\""), "{html}");
    assert!(html.contains(">Oct 2, 2026</time>"), "{html}");
    // Each tag downloads as a ZIP of its files, named by its full ref.
    assert!(
        html.contains("/alice/project/archive/refs%2Ftags%2Fv1.zip"),
        "{html}"
    );
}

#[test]
fn rejected_merge_methods_form_keeps_each_saved_checkbox() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        ..Default::default()
    };
    let values = std::collections::HashMap::from([
        ("allow_merge_commit".into(), "false".into()),
        ("allow_squash_merge".into(), "true".into()),
        ("allow_rebase_merge".into(), "false".into()),
    ]);
    let html = dioxus_ssr::render_element(rejected_form(
        &repo,
        "merge-methods",
        "At least one merge method must stay allowed",
        &values,
    ));
    let input = |name: &str| {
        html.split("<input")
            .find(|input| input.contains(&format!("name=\"{name}\"")))
            .unwrap()
            .split('>')
            .next()
            .unwrap()
    };
    assert!(input("allow_squash_merge").contains("checked"), "{html}");
    assert!(!input("allow_merge_commit").contains("checked"), "{html}");
    assert!(!input("allow_rebase_merge").contains("checked"), "{html}");
}
#[test]
fn branch_protection_section_lists_rules_and_keeps_a_refused_rule() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let saved = BranchRule {
        pattern: "release/**".into(),
        required_approving_reviews: "2".into(),
        require_status_checks: true,
        required_status_checks: vec!["lint".into(), "ci/build".into()],
        allow_force_pushes: true,
        restrict_pushes: true,
        push_allowances: vec!["bob".into()],
        ..BranchRule::default()
    };
    let settings = Settings {
        tab: "branches".into(),
        branch_rules: vec![saved],
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &settings));
    assert!(
        settings_nav_link(&html, "branches").contains("aria-current=\"page\""),
        "{html}"
    );
    assert!(html.contains("/_forms/protection-save"), "{html}");
    assert!(html.contains("/_forms/protection-delete"), "{html}");
    for summary in [
        "2 approvals required",
        "Status checks: lint, ci/build",
        "Pushes limited to bob",
        "Pull request required",
        "Applies to administrators",
        "No deletions",
    ] {
        assert!(html.contains(summary), "{summary}: {html}");
    }
    assert!(!html.contains("No force pushes"), "{html}");

    // The add form starts with the API defaults: a pull request, administrators included, and
    // both blocks on.
    let add = html
        .rsplit_once("/_forms/protection-save\"")
        .expect("the add form")
        .1;
    let checked = |name: &str| {
        add.split("<input")
            .find(|input| input.contains(&format!("name=\"{name}\"")))
            .unwrap_or_else(|| panic!("{name} in the add form"))
            .split('>')
            .next()
            .unwrap()
            .contains("checked")
    };
    for name in [
        "require_pull_request",
        "dismiss_stale_reviews",
        "enforce_admins",
        "block_force_pushes",
        "block_deletions",
    ] {
        assert!(checked(name), "{name}: {add}");
    }
    for name in [
        "require_status_checks",
        "require_up_to_date_branch",
        "restrict_pushes",
    ] {
        assert!(!checked(name), "{name}: {add}");
    }

    // A refused add comes back in the add form, with the values it was sent with.
    let refused = Settings {
        tab: "branches".into(),
        branch_rules: vec![BranchRule {
            pattern: "release/**".into(),
            ..BranchRule::default()
        }],
        rule_draft: Some(RuleDraft {
            rule: BranchRule {
                pattern: "bad..name".into(),
                required_approving_reviews: "two".into(),
                push_allowances: vec!["bob".into(), "carol".into()],
                ..BranchRule::default()
            },
            message: "Use a branch name or a pattern with *, ** or ?".into(),
        }),
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &refused));
    let add = html
        .rsplit_once("/_forms/protection-save\"")
        .expect("the add form")
        .1;
    assert!(
        add.contains("Use a branch name or a pattern with *, ** or ?"),
        "{add}"
    );
    assert!(add.contains("value=\"bad..name\""), "{add}");
    assert!(add.contains("value=\"two\""), "{add}");
    assert!(add.contains("bob, carol"), "{add}");
}

#[test]
fn force_push_allowances_show_on_the_card_and_in_its_form() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let saved = BranchRule {
        pattern: "main".into(),
        allow_force_pushes: false,
        force_push_allowances: vec!["bob".into(), "carol".into()],
        ..BranchRule::default()
    };
    let settings = Settings {
        tab: "branches".into(),
        branch_rules: vec![saved],
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &settings));
    assert!(html.contains("No force pushes"), "{html}");
    assert!(
        html.contains("Force pushes allowed for bob, carol"),
        "{html}"
    );
    assert!(html.contains("name=\"force_push_allowances\""), "{html}");
    assert!(html.contains("name=\"force_push_extra\""), "{html}");
}

#[test]
fn force_push_picker_offers_the_viewer_first_then_suggested_people() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let person = |username: &str, access: &str| Collaborator {
        username: username.into(),
        access: access.into(),
    };
    // Dave is listed by the rule but no longer has write access, so he is still checked.
    let settings = Settings {
        tab: "branches".into(),
        viewer: "alice".into(),
        force_push_suggestions: vec![
            person("bob", "write"),
            person("Bob", "write"),
            person("carol", "admin"),
        ],
        branch_rules: vec![BranchRule {
            pattern: "main".into(),
            allow_force_pushes: false,
            force_push_allowances: vec!["bob".into(), "dave".into()],
            ..BranchRule::default()
        }],
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &settings));
    // The saved rule's card comes before the add form, so the first checkbox for a name is its.
    let checked = |value: &str| {
        let input = html
            .split("<input")
            .find(|input| {
                input.contains("name=\"force_push_allowances\"")
                    && input.contains(&format!("value=\"{value}\""))
            })
            .unwrap_or_else(|| panic!("{value} in the picker: {html}"));
        input.split('>').next().unwrap().contains("checked")
    };
    assert!(html.contains("You (@alice)"), "{html}");
    assert!(
        html.find("You (@alice)") < html.find("value=\"bob\""),
        "{html}"
    );
    assert!(!checked("alice"), "{html}");
    assert!(checked("bob"), "{html}");
    assert!(
        !checked("Bob"),
        "case-distinct writer must not inherit the allowance: {html}"
    );
    assert!(!checked("carol"), "{html}");
    assert!(checked("dave"), "{html}");
    assert!(html.contains(">admin</small>"), "{html}");
    assert!(
        html.contains("Someone else (usernames, separated by commas)"),
        "{html}"
    );
}

#[test]
fn saved_branch_rule_is_a_card_with_its_chips_and_a_delete_form() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let settings = Settings {
        tab: "branches".into(),
        branch_rules: vec![BranchRule {
            pattern: "release/**".into(),
            required_approving_reviews: "2".into(),
            ..BranchRule::default()
        }],
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &settings));
    let card = html
        .split("<details class=\"repo-rule\"")
        .nth(1)
        .expect("a rule card")
        .split("</details>")
        .next()
        .unwrap();
    // A saved rule stays closed. Its pattern and chips sit in the header; the forms sit in the body.
    assert!(card.split('>').next().unwrap().trim().is_empty(), "{card}");
    let (header, body) = card.split_once("</summary>").expect("the card header");
    assert!(header.contains("release/**"), "{card}");
    assert!(header.contains("repo-rule-chip"), "{card}");
    assert!(header.contains("2 approvals required"), "{card}");
    assert!(body.contains("/_forms/protection-save"), "{card}");
    assert!(body.contains("/_forms/protection-delete"), "{card}");
    assert!(!header.contains("protection-delete"), "{card}");
}

#[test]
fn refused_branch_rule_save_opens_its_card_and_shows_the_alert() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let refused = Settings {
        tab: "branches".into(),
        branch_rules: vec![BranchRule {
            pattern: "release/**".into(),
            ..BranchRule::default()
        }],
        rule_draft: Some(RuleDraft {
            rule: BranchRule {
                pattern: "release/**".into(),
                required_approving_reviews: "x".into(),
                ..BranchRule::default()
            },
            message: "Required approvals must be a whole number.".into(),
        }),
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings::settings(&repo, &refused));
    let card = html
        .split("<details class=\"repo-rule\"")
        .nth(1)
        .expect("a rule card");
    assert!(card.split('>').next().unwrap().contains("open"), "{card}");
    let edit = card
        .split("<form")
        .find(|form| form.contains("Required approvals must be a whole number."))
        .expect("the edit form of the refused rule");
    assert!(edit.contains("role=\"alert\""), "{edit}");
    assert!(edit.contains("value=\"release/**\""), "{edit}");
    assert!(edit.contains("value=\"x\""), "{edit}");
}
#[test]
fn failed_save_heading_says_whether_the_file_was_saved() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let saved = Editor {
        branch: "main".into(),
        path: "file.md".into(),
        content: "committed text".into(),
        error: Some("The file was saved, but pull request refresh failed.".into()),
        saved: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &saved));
    assert!(html.contains("File saved; follow-up failed"));
    assert!(!html.contains("Changes were not saved"));
    let failed = Editor {
        saved: false,
        ..saved
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &failed));
    assert!(html.contains("Changes were not saved"));
    assert!(!html.contains("File saved"));
}
#[test]
fn editor_keeps_long_lines_scrollable_and_gutter_aligned_at_every_width() {
    let css = include_str!("../../components/native.css");
    let desktop = css
        .split_once(".repo-content textarea.repo-editor-textarea {")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(desktop.contains("white-space: pre;"), "{desktop}");
    assert!(desktop.contains("overflow-x: auto;"), "{desktop}");
    assert!(!css
        .contains("textarea.repo-editor-textarea, .repo-editor-highlight { white-space: pre-wrap"));
    assert!(desktop.contains("overflow-wrap: normal;"));
}

#[test]
fn phone_menus_scroll_and_keep_engagement_and_branch_controls_available() {
    let css = include_str!("../../components/native.css");
    let phone = css
        .split_once("@container (min-width: 356px)")
        .unwrap()
        .1
        .split_once(".native-menu {")
        .unwrap()
        .0;
    assert!(!phone.contains(".repo-tabs-more { display: none; }"));
    assert!(css.contains("max-height: calc(100dvh - 72px);\n    overflow-y: auto;"));
    let repo_css = include_str!("style.css");
    assert!(!repo_css.contains(".repo-home-actions > :is(.repo-goto-phone"));
    assert!(!repo_css
        .contains(".repo-file-actions > :is(.repo-ref-link, .repo-goto-phone, .repo-add-menu)"));
}

#[test]
fn settings_anchors_clear_the_header_and_ref_confirmation_is_a_dialog() {
    let css = include_str!("../../components/native.css");
    // Anchors and the sticky settings nav must clear the sticky site header and its tab row.
    assert!(css.contains("--site-header-height: 52px;"));
    assert!(css.contains("--repo-tabs-height: 44px;"));
    assert!(css.contains("top: calc(var(--site-header-height) + var(--repo-tabs-height) + 16px);"));
    assert!(css.contains(
        "scroll-margin-top: calc(var(--site-header-height) + var(--repo-tabs-height) + 16px);"
    ));
    // The ref delete confirmation is a fixed dialog, so it is not clipped by its panel and moves nothing.
    let modal = css
        .split_once(".repo-confirm-modal {")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(modal.contains("position: fixed"));
}
#[test]
fn settings_collaborator_form_is_only_for_personal_repositories() {
    let mut repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_admin: true,
        can_manage_collaborators: true,
        ..Default::default()
    };
    let personal = settings_tab_html(&repo, "collaborators");
    assert!(personal.contains("action=\"/alice/project/_forms/collaborator\""));
    assert!(personal.contains("tab=collaborators"));
    // Organization repositories keep their other settings, without personal collaborator controls.
    repo.can_manage_collaborators = false;
    let organization = settings_tab_html(&repo, "collaborators");
    assert!(!organization.contains("/_forms/collaborator"));
    assert!(!organization.contains("/_forms/remove-collaborator"));
    assert!(!organization.contains("tab=collaborators"));
    // Labels are managed on their own page, which the settings navigation links to.
    assert!(settings_tab_html(&repo, "general").contains("href=\"/alice/project/labels\""));
}
#[test]
fn protocol_links_use_existing_archive_and_slash_ref_raw_adapter() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let page = TreePage {
        languages: Vec::new(),
        releases: None,
        reference: "feature/x".into(),
        path: String::new(),
        branches: vec![],
        tags: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        folders: Vec::new(),
        permalink: String::new(),
        empty: false,
        http_clone: "http://example.invalid".into(),
        ssh_clone: "ssh://git@example.invalid".into(),
        references: Default::default(),
    };
    let html = dioxus_ssr::render_element(tree(&repo, &page));
    assert!(html.contains("href=\"/alice/project/archive/feature%2Fx.zip\""));
    assert!(html.contains("href=\"/alice/project/archive/feature%2Fx.tar.gz\""));
}
#[test]
fn a_tag_view_downloads_the_tag_by_its_full_ref_unless_a_branch_has_its_name() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let view = |branches: Vec<String>| TreePage {
        languages: Vec::new(),
        releases: None,
        reference: "v1".into(),
        path: String::new(),
        branches,
        tags: vec!["v1".into()],
        entries: vec![],
        readme: None,
        latest_commit: None,
        folders: Vec::new(),
        permalink: String::new(),
        empty: false,
        http_clone: "http://example.invalid".into(),
        ssh_clone: "ssh://git@example.invalid".into(),
        references: Default::default(),
    };
    let tag_only = dioxus_ssr::render_element(tree(&repo, &view(vec![])));
    assert!(tag_only.contains("href=\"/alice/project/archive/refs%2Ftags%2Fv1.zip\""));
    assert!(tag_only.contains("href=\"/alice/project/archive/refs%2Ftags%2Fv1.tar.gz\""));
    // With a branch of the same name, the view shows that branch, so its links name the branch.
    let shadowed = dioxus_ssr::render_element(tree(&repo, &view(vec!["v1".into()])));
    assert!(shadowed.contains("href=\"/alice/project/archive/v1.zip\""));
    assert!(!shadowed.contains("refs%2Ftags%2Fv1"));
}

#[test]
fn empty_repository_quick_setup_is_shown_only_to_writers() {
    let mut page = TreePage {
        languages: Vec::new(),
        releases: None,
        reference: "main".into(),
        path: String::new(),
        branches: vec![],
        tags: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        folders: Vec::new(),
        permalink: String::new(),
        empty: true,
        http_clone: "https://git.example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        references: Default::default(),
    };
    let chrome = |can_write: bool| RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        signed_in: true,
        can_write,
        csrf_token: "csrf-fixture".into(),
        current_path: "/alice/project".into(),
        ..Default::default()
    };
    let html =
        crate::webui::tests::in_scope(|| dioxus_ssr::render_element(tree(&chrome(true), &page)));
    assert!(html.contains("This repository is empty"));
    assert!(html.contains("Create new file"));
    // The two actions share a row with a gap, and creating a file is the primary one.
    assert!(html.contains("class=\"repo-empty-actions\""));
    assert!(html.contains("class=\"btn btn-primary\">Create new file</a>"));
    assert!(html.contains("class=\"btn btn-secondary\">Upload files</a>"));
    assert!(html.contains("Quick setup"));
    assert!(html.contains("Create a new repository on the command line"));
    assert!(html.contains("Push an existing repository from the command line"));
    assert!(html.contains("<pre class=\"repo-quick-setup-code\">"));
    assert!(html.contains("git init"));
    assert!(html.contains("git add ."));
    assert!(
        html.contains("git branch -M &#39;main&#39;")
            || html.contains("git branch -M &#x27;main&#x27;")
            || html.contains("git branch -M 'main'")
    );
    assert!(
        html.contains(
            "git remote add origin &#39;https://git.example.invalid/alice/project.git&#39;"
        ) || html.contains("git remote add origin 'https://git.example.invalid/alice/project.git'")
    );
    assert!(
        html.contains("git push -u origin &#39;main&#39;")
            || html.contains("git push -u origin 'main'")
    );
    // The body already offers the two choices, so the repository toolbar has no Add file menu.
    assert!(!html.contains("class=\"native-menu repo-menu repo-add-menu\""));

    // With an SSH URL, the SSH choice has its own commands pushing to that URL.
    page.ssh_clone = "ssh://git@git.example.invalid/alice/project.git".into();
    let both =
        crate::webui::tests::in_scope(|| dioxus_ssr::render_element(tree(&chrome(true), &page)));
    assert!(
        both.contains(
            "git remote add origin &#39;ssh://git@git.example.invalid/alice/project.git&#39;"
        ) || both
            .contains("git remote add origin 'ssh://git@git.example.invalid/alice/project.git'")
            || both.contains(
                "git remote add origin &#x27;ssh://git@git.example.invalid/alice/project.git&#x27;"
            )
    );
    page.ssh_clone = String::new();

    let reader =
        crate::webui::tests::in_scope(|| dioxus_ssr::render_element(tree(&chrome(false), &page)));
    assert!(reader.contains("This repository is empty"));
    assert!(!reader.contains("Create new file"));
    assert!(!reader.contains("Quick setup"));
    assert!(!reader.contains("git init"));
}

#[test]
fn add_file_menus_hold_both_choices_on_the_home_and_on_a_folder() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        // Both are native menus, so the shared menu script opens and closes them like the others.
        let add_file_menus = |html: &str| -> Vec<String> {
            html.split("class=\"native-menu repo-menu repo-add-menu\"")
                .skip(1)
                .map(|rest| rest.split_once("</details>").unwrap().0.to_owned())
                .collect()
        };
        // The repository home has no Files panel, so its toolbar holds the only Add file menu.
        page.path.clear();
        let home = dioxus_ssr::render_element(tree(&repo, &page));
        let menus = add_file_menus(&home);
        assert_eq!(menus.len(), 1);
        assert!(menus[0].contains("class=\"btn btn-secondary\""));
        assert!(menus[0].contains("Add file"));
        // A folder draws one in its title row and one in its Files panel.
        page.path = "docs".into();
        let folder = dioxus_ssr::render_element(tree(&repo, &page));
        let menus = add_file_menus(&folder);
        assert_eq!(menus.len(), 2);
        for menu in &menus {
            assert!(menu.contains("<summary"));
            assert!(menu.contains("class=\"repo-menu-panel\""));
            assert!(menu.contains("Create new file"));
            assert!(menu.contains("Upload files"));
        }
        assert!(menus
            .iter()
            .any(|menu| menu.contains("class=\"btn btn-secondary\"") && menu.contains("Add file")));
        assert!(menus
            .iter()
            .any(|menu| menu.contains("aria-label=\"Add file\"")));
    });
}

#[test]
fn tag_views_offer_no_file_writing_controls() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.reference = "v1/release".into();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(!html.contains("Create new file"), "{html}");
        assert!(!html.contains("Edit README"), "{html}");
    });
}

#[test]
fn repository_home_has_no_files_panel() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        let home = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(home.contains("class=\"repo-home-layout\""));
        assert!(!home.contains("class=\"repo-files-panel\""));
        assert!(!home.contains("id=\"repo-files-toggle\""));
    });
}

#[test]
fn folder_and_file_pages_have_a_files_panel() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path = "docs".into();
        let folder = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(folder.contains("class=\"repo-blob-layout\""));
        assert!(folder.contains("class=\"repo-files-panel\""));
        assert!(folder.contains("id=\"repo-files-toggle\""));
        let Page::Blob { repo, page } = fixtures().remove(1) else {
            unreachable!()
        };
        let file = dioxus_ssr::render_element(blob(&repo, &page));
        assert!(file.contains("class=\"repo-files-panel\""));
        assert!(file.contains("id=\"repo-files-toggle\""));
    });
}

#[test]
fn branch_menu_marks_the_default_branch_and_no_other() {
    crate::webui::tests::in_scope(|| {
        let Page::Blob { mut repo, mut page } = fixtures().remove(1) else {
            unreachable!()
        };
        page.branches = vec!["feature/x".into(), "main".into()];
        repo.default_branch = "main".into();
        let html = dioxus_ssr::render_element(blob(&repo, &page));
        assert_eq!(
            html.matches("class=\"repo-label-neutral repo-ref-default\"")
                .count(),
            1,
            "{html}"
        );
        let badge = html
            .find("class=\"repo-label-neutral repo-ref-default\"")
            .expect("the default badge");
        assert!(
            html.find("repo-ref-option-name\" title=\"main\">main<")
                .is_some_and(|main| main < badge),
            "{html}"
        );
    });
}

#[test]
fn clone_menu_shows_one_url_at_a_time_with_an_https_and_ssh_choice() {
    let Page::Tree { repo, page } = fixtures().remove(0) else {
        unreachable!()
    };
    let both = dioxus_ssr::render_element(tree(&repo, &page));
    assert!(both.contains("id=\"clone-menu-https\""));
    assert!(both.contains("id=\"clone-menu-ssh\""));
    assert!(both.contains("name=\"clone-menu-protocol\""));
    assert!(both.contains("for=\"clone-menu-ssh\""));
    assert!(both.contains("aria-label=\"HTTPS clone URL\""));
    assert!(both.contains("aria-label=\"SSH clone URL\""));
    let http_only = TreePage {
        ssh_clone: String::new(),
        ..page
    };
    let html = dioxus_ssr::render_element(tree(&repo, &http_only));
    assert!(html.contains("aria-label=\"HTTPS clone URL\""));
    assert!(!html.contains("clone-menu-protocol"));
    assert!(!html.contains("aria-label=\"SSH clone URL\""));
}

/// The start tag of the element whose accessible name is `label`.
fn start_tag_named<'a>(html: &'a str, label: &str) -> &'a str {
    let at = html
        .find(&format!("aria-label=\"{label}\""))
        .unwrap_or_else(|| panic!("no element named {label}: {html}"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

#[test]
fn phone_keeps_watch_fork_and_star_in_the_more_menu() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, .. } = fixtures().remove(0) else {
            unreachable!()
        };
        // The page body no longer carries a row of them.
        let body =
            dioxus_ssr::render_element(chrome(&repo, rsx! { section { id: "body-marker" } }));
        assert!(
            !body.contains("class=\"repo-engagement repo-engagement"),
            "{body}"
        );
        // They sit in the More menu of the tab strip: three native forms or links.
        let tabs = dioxus_ssr::render_element(repo_tabs(&repo));
        let more = tabs
            .split_once("repo-more-engagement")
            .expect("the More menu block")
            .1
            .split_once("repo-engagement flex")
            .expect("the strip's own buttons follow the More menu")
            .0;
        assert_eq!(
            more.matches("class=\"repo-engagement-button").count(),
            3,
            "{more}"
        );
    });
}

#[test]
fn repository_home_toolbar_is_one_row_of_branch_find_add_and_code() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        let toolbar = html
            .split_once("class=\"repo-home-toolbar\"")
            .expect("the toolbar")
            .1
            .split_once("<table")
            .expect("the file list")
            .0;
        // The branch picker, then Go to file, Add file and Code.
        let order = [
            "repo-ref-menu",
            "repo-goto-phone",
            "repo-add-menu",
            "repo-code-menu",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|marker| {
                toolbar
                    .find(marker)
                    .unwrap_or_else(|| panic!("{marker} missing: {toolbar}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{toolbar}"
        );
        // On a phone the icon opens the finder page. The desktop box stays beside the branch picker.
        let finder = format!("href=\"/alice/project/find/{}\"", page.reference);
        assert!(toolbar.contains(&finder), "{toolbar}");
        assert!(toolbar.contains("aria-label=\"Go to file\""), "{toolbar}");
        assert!(toolbar.contains("class=\"repo-files-search\""), "{toolbar}");
    });
}

#[test]
fn folder_title_row_offers_go_to_file_add_file_and_code_in_one_row() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path = "docs".into();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        let actions = html
            .split_once("class=\"repo-file-actions\"")
            .expect("the title row actions")
            .1;
        let order = ["repo-goto-phone", "repo-add-menu", "repo-code-menu"];
        let positions: Vec<usize> = order
            .iter()
            .map(|marker| {
                actions
                    .find(marker)
                    .unwrap_or_else(|| panic!("{marker} missing: {actions}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{actions}"
        );
        // The Add file and Code labels are hidden on a phone by CSS; their icons stay.
        assert!(actions.contains("class=\"repo-button-label\""), "{actions}");
    });
}

#[test]
fn icon_buttons_name_themselves_in_a_tooltip_and_not_in_a_title() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path = "docs".into();
        let folder = dioxus_ssr::render_element(tree(&repo, &page));
        for label in [
            "Copy path",
            "Copy HTTPS URL",
            "Hide the file tree",
            "Add file",
            "Go to file",
        ] {
            let tag = start_tag_named(&folder, label);
            assert!(tag.contains("icon-tooltip"), "{tag}");
            assert!(!tag.contains("title="), "{tag}");
        }
        let Page::Blob { repo, page } = fixtures().remove(1) else {
            unreachable!()
        };
        let file = dioxus_ssr::render_element(blob(&repo, &page));
        for label in [
            "Copy preview",
            "Download",
            "Edit file",
            "Copy path",
            "Hide the file tree",
        ] {
            let tag = start_tag_named(&file, label);
            assert!(tag.contains("icon-tooltip"), "{tag}");
            assert!(!tag.contains("title="), "{tag}");
        }
    });
}

#[test]
fn go_to_file_box_has_a_slash_hint_and_submits_to_the_find_page_without_scripts() {
    let Page::Tree { repo, page } = fixtures().remove(0) else {
        unreachable!()
    };
    let html = dioxus_ssr::render_element(tree(&repo, &page));
    let find = file_url(&repo.base(), "find", &page.reference, "");
    assert!(html.contains(&format!("action=\"{find}\"")));
    assert!(html.contains("aria-keyshortcuts=\"/\""));
    assert!(html.contains("<span class=\"repo-files-key\""));
    // The dropdown of matches is drawn once the page runs in the browser.
    assert!(!html.contains("class=\"repo-files-results\""));
}

#[test]
fn go_to_file_matches_loaded_names_and_paths_in_path_order() {
    let entry = |path: &str, kind: &str| TreeEntry {
        name: path.rsplit('/').next().unwrap_or(path).into(),
        path: path.into(),
        kind: kind.into(),
        size: None,
        last_commit: None,
    };
    let folders = vec![
        FolderListing {
            path: String::new(),
            entries: vec![entry("README.md", "file"), entry("docs", "tree")],
        },
        FolderListing {
            path: "docs".into(),
            entries: vec![
                entry("docs/guide-images", "tree"),
                entry("docs/Guide.md", "file"),
            ],
        },
    ];
    let found: Vec<&str> = matching_entries(&folders, "guide")
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(found, ["docs/Guide.md", "docs/guide-images"]);
    assert_eq!(matching_entries(&folders, "readme").len(), 1);
    assert!(matching_entries(&folders, "").is_empty());
}

#[test]
fn repository_pages_carry_no_inline_css() {
    crate::webui::tests::in_scope(|| {
        let mut renderer = dioxus_ssr::Renderer::new();
        renderer.pre_render = true;
        let html = renderer.render_element(render(fixtures()[0].clone()));
        assert!(!html.contains("<style"));
    });
}

#[test]
fn restored_file_browser_integrates_commit_rows_and_repository_description() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        page.entries.insert(
            0,
            TreeEntry {
                name: "z-last.txt".into(),
                path: "z-last.txt".into(),
                kind: "file".into(),
                size: Some(17),
                last_commit: None,
            },
        );
        page.entries.push(TreeEntry {
            name: "src".into(),
            path: "src".into(),
            kind: "tree".into(),
            size: None,
            last_commit: None,
        });
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        let browser = html.split_once("class=\"repo-file-browser ").unwrap().1;
        assert!(
            browser.find("class=\"repo-latest-commit ").unwrap() < browser.find("<table").unwrap()
        );
        let table = browser
            .split_once("<table")
            .unwrap()
            .1
            .split_once("</table>")
            .unwrap()
            .0;
        assert!(
            table.find("/tree/refs/heads/feature/x/src").unwrap()
                < table
                    .find("/blob/refs/heads/feature/x/docs/ReadMe.md")
                    .unwrap()
        );
        assert!(
            table
                .find("/blob/refs/heads/feature/x/docs/ReadMe.md")
                .unwrap()
                < table.find("/blob/refs/heads/feature/x/z-last.txt").unwrap()
        );
        assert!(table.contains("sr-only"));
        assert!(table.contains("title=\"2026-10-02T00:00:00Z\""));
        assert!(table.contains("datetime=\"2026-10-02T00:00:00Z\""));
        assert!(!table.contains("▸"));
        assert!(html.contains("repo-code-button"));
        assert!(html.contains("Create new file"));
        assert!(html.contains("Download ZIP"));
        assert!(html.contains("repo-readme-header"));
        // The root shows its description in the About column and has no "…" menu. It has no Files
        // panel either, since its file list is the only tree.
        assert!(html.contains("An actual description"));
        assert!(html.contains("class=\"repo-about\""));
        assert!(!html.contains("class=\"repo-files-panel\""));
        assert!(!html.contains("Copy permalink"));
        page.path = "docs".into();
        let subtree = dioxus_ssr::render_element(tree(&repo, &page));
        // A folder has the same two-column layout and Files panel as the root, without the
        // description.
        assert!(subtree.contains("class=\"repo-blob-layout\""));
        assert!(subtree.contains("class=\"repo-files-panel\""));
        assert!(subtree.contains("aria-label=\"File path\""));
        assert!(!subtree.contains("class=\"repo-description\""));
        assert!(!subtree.contains("class=\"repo-about\""));
    });
}

#[test]
fn repository_home_about_lists_topics_and_website_and_offers_the_gear_to_writers_only() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { mut repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        // The About column belongs to the repository home, which is the root folder.
        page.path.clear();
        repo.website = "https://example.com/project".into();
        repo.topics = vec!["git".into(), "rust".into()];
        repo.can_write = true;
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(html.contains("href=\"https://example.com/project\""));
        assert!(html.contains("href=\"/explore?topic=git\""));
        assert!(html.contains("href=\"/explore?topic=rust\""));
        // The gear opens a form that posts the description, website and topics to the About action.
        assert!(html.contains("class=\"repo-about-edit\""));
        assert!(html.contains("action=\"/alice/project/_forms/about\""));
        assert!(html.contains("name=\"topics\""));
        assert!(html.contains("git, rust"));

        repo.can_write = false;
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(html.contains("href=\"/explore?topic=git\""));
        assert!(!html.contains("class=\"repo-about-edit\""));
        assert!(!html.contains("_forms/about"));

        // Only an http or https address is linked. The link shows its host and keeps the address
        // as its tooltip.
        repo.website = "https://example.com/project".into();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(
            html.contains("title=\"https://example.com/project\""),
            "{html}"
        );
        assert!(html.contains("<span>example.com</span>"), "{html}");
        repo.website = "javascript:alert(1)".into();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(!html.contains("javascript:"));
    });
}

#[test]
fn readme_outline_appears_only_with_two_or_more_headings() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        page.readme = Some("# demo\n\nOne heading only.\n".into());
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(html.contains("id=\"readme\""), "{html}");
        assert!(
            !html.contains("class=\"native-menu repo-menu repo-outline-menu\""),
            "{html}"
        );
        page.readme = Some("# demo\n\n## Setup\n\nSteps.\n".into());
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(
            html.contains("class=\"native-menu repo-menu repo-outline-menu\""),
            "{html}"
        );
    });
}

#[test]
fn watch_button_names_its_level_and_a_custom_level_lists_its_events() {
    crate::webui::tests::in_scope(|| {
        let mut repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            csrf_token: "csrf-fixture".into(),
            signed_in: true,
            ..Default::default()
        };
        let closed = |repo: &RepoChrome| {
            dioxus_ssr::render_element(repo_engagement(repo, "repo-engagement"))
        };
        repo.watch_level = "custom".into();
        repo.watch_events = "pulls".into();
        let html = closed(&repo);
        assert!(
            html.contains("class=\"repo-engagement-text\">Custom</span>"),
            "{html}"
        );
        assert!(html.contains("title=\"Custom: Pull requests\""), "{html}");
        // The event choices are always in the menu; CSS shows them while Custom is chosen.
        assert!(html.contains("class=\"repo-watch-events\""), "{html}");
        repo.watch_level = "watching".into();
        repo.watch_events = String::new();
        let html = closed(&repo);
        assert!(
            html.contains("class=\"repo-engagement-text\">Watching</span>"),
            "{html}"
        );
        assert!(!html.contains("Custom:"), "{html}");
        repo.watch_level = "ignoring".into();
        assert!(closed(&repo).contains("class=\"repo-engagement-text\">Ignoring</span>"));
        repo.watch_level = String::new();
        assert!(closed(&repo).contains("class=\"repo-engagement-text\">Watch</span>"));
    });
}

#[test]
fn editing_a_file_has_one_commit_button_and_no_second_branch_field() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        signed_in: true,
        can_write: true,
        ..Default::default()
    };
    let editor = Editor {
        path: "old.rs".into(),
        branch: "main".into(),
        expected_revision: "abc".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::editor(&repo, &editor));
    assert_eq!(html.matches("value=\"save\"").count(), 1, "{html}");
    assert!(html.contains("name=\"branch\" value=\"main\""), "{html}");
    assert!(!html.contains("<span>Branch</span>"), "{html}");
    assert!(html.contains("repo-commit-branch"), "{html}");
}

#[test]
fn repository_home_about_shows_the_language_bar_only_when_there_are_languages() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        assert!(!html.contains("class=\"language-bar\""), "{html}");

        page.languages = vec![
            LanguageShare {
                name: "Rust".into(),
                percentage: 92.5,
                color: "#dea584".into(),
            },
            LanguageShare {
                name: "Shell".into(),
                percentage: 7.5,
                color: "#89e051".into(),
            },
        ];
        let html = dioxus_ssr::render_element(tree(&repo, &page));
        // The bar belongs to the About panel, with its legend and a tooltip for each segment.
        let about = html.find("aria-label=\"About\"").unwrap();
        let bar = html.find("class=\"language-bar\"").unwrap();
        assert!(about < bar, "the bar is in the About panel: {html}");
        assert!(html.contains("title=\"Rust 92.5%\""), "{html}");
        assert!(html.contains("title=\"Shell 7.5%\""), "{html}");
        assert!(html.contains("class=\"language-legend\""), "{html}");
    });
}

#[test]
fn oversized_readme_keeps_its_section_and_file_link_in_the_view_model() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.readme = None;
        let entry = page
            .entries
            .iter_mut()
            .find(|entry| entry.name.eq_ignore_ascii_case("readme.md"))
            .unwrap();
        entry.size = Some(MAX_README_BYTES + 1);
        let link = file_url(&repo.base(), "blob", &page.reference, &entry.path);
        let roundtrip: TreePage =
            serde_json::from_value(serde_json::to_value(&page).unwrap()).unwrap();
        assert_eq!(roundtrip, page);
        let html = dioxus_ssr::render_element(tree(&repo, &roundtrip));
        assert!(html.contains("id=\"readme\""));
        assert!(html.contains("This README is larger than 500 KiB and is not rendered inline."));
        assert!(html.contains(&format!("href=\"{link}\"")));
        assert!(html.contains("View file"));
    });
}

#[test]
fn repo_tabs_keep_the_primary_tabs_in_the_strip_and_list_the_rest_in_more() {
    crate::webui::tests::in_scope(|| {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            can_admin: true,
            current_path: "/alice/project/tags".into(),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(repo_tabs(&repo));
        let (strip, more) = html.split_once("<details").unwrap();
        for href in [
            "/alice/project",
            "/alice/project/issues",
            "/alice/project/pulls",
            "/alice/project/actions",
        ] {
            assert!(strip.contains(&format!("href=\"{href}\"")), "{href}");
        }
        assert!(strip.contains("class=\"repo-tabs-secondary\""));
        assert!(strip.contains("class=\"repo-tab repo-tab-active\""));
        assert!(more.contains("aria-label=\"More repository pages\""));
        assert!(more.contains("repo-tabs-more-current"));
        for href in [
            "/alice/project/commits",
            "/alice/project/branches",
            "/alice/project/tags",
            "/alice/project/settings",
        ] {
            assert!(more.contains(&format!("href=\"{href}\"")), "{href}");
        }
        assert!(!more.contains("href=\"/alice/project/issues\""));
    });
}

#[test]
fn repo_engagement_keeps_native_forms_and_fork_denials() {
    crate::webui::tests::in_scope(|| {
        let mut repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            signed_in: true,
            can_admin: true,
            csrf_token: "csrf-fixture".into(),
            current_path: "/alice/project/settings".into(),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(repo_engagement(&repo, "repo-engagement"));
        let tabs = dioxus_ssr::render_element(repo_tabs(&repo));
        assert!(html.contains("repo-engagement-count"));
        let settings_link = tabs
            .split("<a ")
            .find(|link| {
                link.split_once('>')
                    .unwrap()
                    .0
                    .contains("href=\"/alice/project/settings\"")
            })
            .unwrap()
            .split_once('>')
            .unwrap()
            .0;
        assert!(settings_link.contains("class=\"repo-tab repo-tab-active\""));
        for action in ["watch-level", "star", "fork"] {
            assert!(html.contains(&format!("action=\"/alice/project/_forms/{action}\"")));
        }
        assert_eq!(
            html.matches("<form ").count(),
            html.matches("name=\"csrf_token\"").count()
        );
        // A reader may fork a private repository; the fork keeps it private.
        repo.is_private = true;
        let private = dioxus_ssr::render_element(repo_engagement(&repo, "repo-engagement"));
        assert!(private.contains("action=\"/alice/project/_forms/fork\""));
        repo.is_private = false;
        repo.fork_refusal = Some("Cannot fork your own repository".into());
        repo.is_owner = true;
        let own = dioxus_ssr::render_element(repo_engagement(&repo, "repo-engagement"));
        assert!(!own.contains("/_forms/fork"));
        assert!(own.contains("title=\"Cannot fork your own repository\""));
        repo.is_owner = false;
        repo.fork_refusal = Some("Archived repositories cannot be forked".into());
        let archived = dioxus_ssr::render_element(repo_engagement(&repo, "repo-engagement"));
        assert!(!archived.contains("/_forms/fork"));
        assert!(archived.contains("title=\"Archived repositories cannot be forked\""));
        repo.fork_refusal = None;
        repo.signed_in = false;
        repo.can_admin = false;
        let anonymous = dioxus_ssr::render_element(repo_engagement(&repo, "repo-engagement"));
        assert!(!anonymous.contains("<form "));
        assert!(anonymous.contains("/login?redirect=%2Falice%2Fproject"));
        let anonymous_tabs = dioxus_ssr::render_element(repo_tabs(&repo));
        assert!(!anonymous_tabs.contains("/project/settings"));
    });
}

#[test]
fn forked_repository_notes_its_source_above_the_page_body() {
    crate::webui::tests::in_scope(|| {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            parent_name: Some("bob/project".into()),
            parent_url: Some("/bob/project".into()),
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(chrome(&repo, rsx! { p { "Page body" } }));
        let notice = html
            .split_once("class=\"repo-fork-notice\"")
            .unwrap_or_else(|| panic!("{html}"))
            .1;
        assert!(notice.contains("forked from"), "{html}");
        assert!(notice.contains("href=\"/bob/project\""), "{html}");
        assert!(
            html.find("class=\"repo-fork-notice\"") < html.find("Page body"),
            "{html}"
        );
    });
}

#[test]
fn compact_dates_and_themed_code_keep_real_values_and_escaping() {
    assert_eq!(compact_date("2026-10-02T00:00:00Z"), "Oct 2, 2026");
    assert_eq!(compact_date("not-a-date"), "not-a-date");
    let source = "<span style=\"color:#a5d6ff\"><script>&</script>";
    assert_eq!(themed_highlight(source, "text"), code::escape_code(source));
    let html = themed_highlight("const value = '<script>';\n", "javascript");
    assert!(html.contains("color:var(--repo-code-"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert_eq!(html.matches('\n').count(), 1);
}

#[test]
fn carriage_returns_count_as_code_view_line_breaks() {
    let display = display_text("alpha\rbeta\r\ngamma");
    assert_eq!(display, "alpha\nbeta\ngamma");
    assert_eq!(display.lines().count(), 3);
    let html = themed_highlight(&display, "text");
    assert_eq!(html.matches('\n').count(), 2);
}

#[test]
fn actions_are_repository_local_encoded_paths() {
    let repo = RepoChrome {
        owner: "a b".into(),
        name: "r#".into(),
        ..Default::default()
    };
    assert_eq!(repo.action("star"), "/a%20b/r%23/_forms/star");
}

#[test]
fn markdown_file_reads_like_a_readme_with_a_preview_and_code_switch() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let mut page = BlobPage {
        reference: "main".into(),
        path: "docs/guide.md".into(),
        content: "# Guide\n\nSome text.\n".into(),
        size: 21,
        ..Default::default()
    };
    let preview = dioxus_ssr::render_element(blob(&repo, &page));
    // The page's stylesheet names every class, so the checks look for the elements themselves.
    assert!(preview.contains("<h1"));
    assert!(preview.contains("<div class=\"repo-readme-body\">"));
    assert!(
        preview.contains("href=\"/alice/project/blob/refs/heads/main/docs/guide.md?view=code\"")
    );
    assert!(preview.contains("href=\"/alice/project/raw/refs/heads/main/docs/guide.md\""));
    assert!(!preview.contains("Copy code"));
    assert!(!preview.contains("<pre class=\"repo-line-numbers\""));

    page.code_view = true;
    let code = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(code.contains("Copy code"));
    assert!(code.contains("<pre class=\"repo-line-numbers\""));
    assert!(!code.contains("<div class=\"repo-readme-body\">"));
    assert!(code.contains("href=\"/alice/project/blob/refs/heads/main/docs/guide.md\""));
}

#[test]
fn commit_file_header_names_a_new_file_and_hides_git_header_lines() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "c".repeat(40),
        message: "Add notes".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: Vec::new(),
    };
    let diff = Diff {
        files: vec![DiffFile {
            filename: "notes.txt".into(),
            status: "added".into(),
            additions: 1,
            deletions: 0,
            patch: "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n".into(),
            owners: Vec::new(),
        }],
        additions: 1,
        deletions: 0,
        viewed: None,
    };
    let html = dioxus_ssr::render_element(commit_detail(&repo, &commit, &diff, &[], &[], false));
    assert!(html.contains("aria-label=\"Diff view\""), "{html}");
    assert!(html.contains("view=split"), "{html}");
    assert!(!html.contains("collaboration-split-row"), "{html}");
    // The side-by-side view draws the same patch in two columns.
    let side = dioxus_ssr::render_element(commit_detail(&repo, &commit, &diff, &[], &[], true));
    assert!(side.contains("collaboration-split-row"), "{side}");
    assert!(side.contains("+hello"), "{side}");
    assert!(html.contains("class=\"patch-badge\""));
    assert!(html.contains("New file"));
    // The commit's summary and file tree are the Files changed list of a pull request.
    assert!(html.contains("1 file changed"));
    assert!(html.contains("aria-label=\"Changed files\""));
    assert!(html.contains("href=\"#diff-file-0\""));
    assert!(html.contains("+hello"));
    assert!(!html.contains("diff --git"));
    assert!(!html.contains("new file mode"));
    assert!(!html.contains("--- /dev/null"));
}

#[test]
fn file_header_names_the_code_owners_of_a_changed_file() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "d".repeat(40),
        message: "Touch docs".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: Vec::new(),
    };
    let file = |name: &str, owners: Vec<String>| DiffFile {
        filename: name.into(),
        status: "modified".into(),
        additions: 1,
        deletions: 1,
        patch: "-old\n+new\n".into(),
        owners,
    };
    let diff = Diff {
        files: vec![
            file("docs/guide.md", vec!["@alice".into(), "@org/docs".into()]),
            file("README.md", Vec::new()),
        ],
        additions: 2,
        deletions: 2,
        viewed: None,
    };
    let html = dioxus_ssr::render_element(commit_detail(&repo, &commit, &diff, &[], &[], false));
    assert!(html.contains("Owned by @alice, @org/docs"), "{html}");
    assert_eq!(html.matches("Owned by").count(), 1, "{html}");
}

#[test]
fn new_branch_form_starts_from_a_branch_with_the_default_first() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        csrf_token: "token".into(),
        ..Default::default()
    };
    let main = "a".repeat(40);
    let topic = "b".repeat(40);
    let branches = vec![
        RefRow {
            name: "main".into(),
            sha: main.clone(),
            message: None,
            is_default: true,
            ahead_behind: None,
            updated: None,
            pull: None,
        },
        RefRow {
            name: "topic".into(),
            sha: topic.clone(),
            message: None,
            is_default: false,
            ahead_behind: None,
            updated: None,
            pull: None,
        },
    ];
    let html = dioxus_ssr::render_element(refs(&repo, &branches, false, &branches, "", "", 0));
    assert!(!html.contains("Source commit"));
    assert!(html.contains("name=\"sha\""));
    assert!(html.contains("main (aaaaaaa)"));
    assert!(html.contains("topic (bbbbbbb)"));
    assert!(html.find(&format!("value=\"{main}\"")) < html.find(&format!("value=\"{topic}\"")));
    let main_option = html
        .split_once(&format!("value=\"{main}\""))
        .unwrap()
        .1
        .split('>')
        .next()
        .unwrap();
    let topic_option = html
        .split_once(&format!("value=\"{topic}\""))
        .unwrap()
        .1
        .split('>')
        .next()
        .unwrap();
    assert!(main_option.contains("selected"), "{main_option}");
    assert!(!topic_option.contains("selected"), "{topic_option}");
}

#[test]
fn blame_page_gives_each_run_one_hunk_cell_and_a_linkable_line_per_row() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project/blame/main/docs/a.txt".into(),
        ..Default::default()
    };
    let page = BlamePage {
        reference: "main".into(),
        path: "docs/a.txt".into(),
        content: "<script>x</script>\r\nTWO\nthree\n".into(),
        size: 33,
        hunks: vec![
            BlameHunk {
                first_line: 1,
                line_count: 1,
                sha: "abcdef1234567890".into(),
                author: "Alice".into(),
                author_date: "2026-10-02T00:00:00Z".into(),
                summary: "Add the file".into(),
            },
            BlameHunk {
                first_line: 2,
                line_count: 2,
                sha: "1234567890abcdef".into(),
                author: "Bob".into(),
                author_date: "2026-10-03T00:00:00Z".into(),
                summary: "Shout the second line".into(),
            },
        ],
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blame(&repo, &page));
    // One hunk cell per run, spanning the run's rows; one number cell per line.
    assert_eq!(html.matches("class=\"repo-blame-hunk\"").count(), 2);
    assert!(html.contains("rowspan=\"1\""));
    assert!(html.contains("rowspan=\"2\""));
    assert_eq!(html.matches("class=\"repo-blame-number\"").count(), 3);
    assert!(html.contains("id=\"L1\""));
    assert!(html.contains("id=\"L3\""));
    assert!(html.contains("href=\"#L2\""));
    // Neighbouring runs take turns at the shade.
    assert_eq!(
        html.matches("class=\"repo-blame-row repo-blame-shaded\"")
            .count(),
        1
    );
    assert!(html.contains("href=\"/alice/project/commit/abcdef1234567890\""));
    assert!(html.contains(">abcdef1</a>"));
    assert!(html.contains("title=\"Shout the second line\""));
    // Each run names its author with the author's picture, and its date sits on the second line.
    assert!(html.contains("src=\"/avatars/Alice\""));
    assert!(html.contains("src=\"/avatars/Bob\""));
    assert!(html.contains("class=\"repo-blame-author\">Alice<"));
    assert!(html.contains("class=\"repo-blame-meta\""));
    assert!(html.contains("href=\"/alice/project/blob/refs/heads/main/docs/a.txt\""));
    assert!(html.contains("href=\"/alice/project/raw/refs/heads/main/docs/a.txt\""));
    // Source stays escaped, and the carriage return does not make a line of its own.
    assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"));
    assert!(!html.contains("<script>x"));
    assert!(html.contains("TWO"));
}

#[test]
fn blame_page_sits_in_the_file_layout_with_the_commit_row_above_the_switch() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let commit = Commit {
        sha: "c".repeat(40),
        message: "Add notes".into(),
        author: "Alice".into(),
        author_date: "2026-10-02T00:00:00Z".into(),
        committer: "Alice".into(),
        committer_date: "2026-10-02T00:00:00Z".into(),
        parents: Vec::new(),
    };
    let page = BlamePage {
        reference: "main".into(),
        path: "notes.txt".into(),
        content: "hello\n".into(),
        size: 6,
        hunks: vec![BlameHunk {
            first_line: 1,
            line_count: 1,
            sha: commit.sha.clone(),
            author: "Alice".into(),
            author_date: commit.author_date.clone(),
            summary: "Add notes".into(),
        }],
        folders: vec![FolderListing {
            path: String::new(),
            entries: Vec::new(),
        }],
        latest_commit: Some(commit),
        permalink: "https://example.invalid/alice/project/blob/main/notes.txt".into(),
        branches: vec!["main".into()],
        tags: Vec::new(),
        deletable: false,
    };
    let html = dioxus_ssr::render_element(blame(&repo, &page));
    // The Files panel sits beside the file's column, as on the code view.
    assert!(html.contains("class=\"repo-blob-layout\""));
    assert!(html.contains("class=\"repo-files-panel\""));
    assert!(html.contains("class=\"repo-blob-main\""));
    assert!(html.contains("class=\"repo-blame-table\""));
    // The commit row opens the file card, and the Code and Blame switch comes after it.
    let card = html
        .split_once("<section class=\"repo-blob repo-blame border")
        .expect("the file card")
        .1;
    let commit_row = card
        .find("class=\"repo-latest-commit ")
        .expect("the commit row");
    let switch = card
        .find("class=\"repo-view-switch\"")
        .expect("the view switch");
    assert!(commit_row < switch, "{card}");
    assert!(html.contains(">Blame</a>"));
}

#[test]
fn blob_page_links_to_blame_only_for_text_it_can_blame() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project/blob/main/docs/guide.txt".into(),
        ..Default::default()
    };
    let mut page = BlobPage {
        reference: "main".into(),
        path: "docs/guide.txt".into(),
        content: "hello\n".into(),
        size: 6,
        can_blame: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(html.contains("href=\"/alice/project/blame/refs/heads/main/docs/guide.txt\""));
    page.can_blame = false;
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(!html.contains("/blame/"));
}

#[test]
fn file_card_starts_with_the_commit_row_then_the_toolbar_and_content() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let page = BlobPage {
        reference: "main".into(),
        path: "notes.txt".into(),
        content: "hello\n".into(),
        size: 6,
        latest_commit: Some(Commit {
            sha: "c".repeat(40),
            message: "Add notes".into(),
            author: "Alice".into(),
            author_date: "2026-10-02T00:00:00Z".into(),
            committer: "Alice".into(),
            committer_date: "2026-10-02T00:00:00Z".into(),
            parents: Vec::new(),
        }),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    // The commit row, the file toolbar and the code are rows of one card, in that order.
    let card = html
        .split_once("<section class=\"repo-blob border")
        .expect("the file card")
        .1
        .split_once("</section>")
        .expect("the end of the file card")
        .0;
    let commit = card
        .find("class=\"repo-latest-commit ")
        .expect("the commit row");
    let toolbar = card
        .find("class=\"repo-blob-header\"")
        .expect("the file toolbar");
    let code = card.find("class=\"repo-code-body ").expect("the code");
    assert!(commit < toolbar && toolbar < code, "{card}");
    assert!(!html.contains("class=\"repo-blob-commit\""));
}

#[test]
fn upload_links_are_offered_to_writers_only() {
    let chrome = |can_write: bool| RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        signed_in: true,
        can_write,
        csrf_token: "csrf-fixture".into(),
        current_path: "/alice/project".into(),
        ..Default::default()
    };
    let directory = TreePage {
        languages: Vec::new(),
        releases: None,
        reference: "main".into(),
        path: "docs".into(),
        branches: vec!["main".into()],
        tags: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        folders: Vec::new(),
        permalink: String::new(),
        empty: false,
        http_clone: "https://git.example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        references: Default::default(),
    };
    let writer = dioxus_ssr::render_element(tree(&chrome(true), &directory));
    assert!(writer.contains("Upload files"));
    assert!(writer.contains("href=\"/alice/project/upload/refs/heads/main/docs\""));
    let reader = dioxus_ssr::render_element(tree(&chrome(false), &directory));
    assert!(!reader.contains("/upload/"));
    let empty = TreePage {
        empty: true,
        ..directory
    };
    let writer = dioxus_ssr::render_element(tree(&chrome(true), &empty));
    assert!(writer.contains("href=\"/alice/project/upload/refs/heads/main/docs\""));
    let reader = dioxus_ssr::render_element(tree(&chrome(false), &empty));
    assert!(!reader.contains("/upload/"));
}
#[test]
fn go_to_file_filters_the_listing_and_marks_the_first_match() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project/find/main".into(),
        ..Default::default()
    };
    let page = FindPage {
        reference: "main".into(),
        paths: vec![
            "README.md".into(),
            "docs/guide.md".into(),
            "src/lib.rs".into(),
        ],
        truncated: true,
        query: "GUIDE".into(),
    };
    let html = dioxus_ssr::render_element(find(&repo, &page));
    assert!(html.contains("href=\"/alice/project/blob/refs/heads/main/docs/guide.md\""));
    assert!(!html.contains("src/lib.rs"));
    assert!(html.contains("class=\"repo-find-first\""));
    assert!(html.contains("Showing the first 10000 files."));
    let unfiltered = FindPage {
        query: String::new(),
        truncated: false,
        ..page
    };
    let html = dioxus_ssr::render_element(find(&repo, &unfiltered));
    assert!(html.contains("src/lib.rs"));
    assert!(!html.contains("class=\"repo-find-first\""));
    assert!(!html.contains("Showing the first"));
}

#[test]
fn go_to_file_names_its_ref_counts_the_matches_and_says_when_none_match() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project/find/main".into(),
        ..Default::default()
    };
    let page = FindPage {
        reference: "main".into(),
        paths: vec!["README.md".into(), "docs/guide.md".into()],
        truncated: false,
        query: String::new(),
    };
    let html = dioxus_ssr::render_element(find(&repo, &page));
    assert!(html.contains("Go to file on"), "{html}");
    assert!(html.contains("href=\"/alice/project/tree/main\""), "{html}");
    assert!(html.contains(">2 files<"), "{html}");
    assert!(!html.contains("class=\"repo-empty"), "{html}");

    let one = FindPage {
        query: "guide".into(),
        ..page.clone()
    };
    let html = dioxus_ssr::render_element(find(&repo, &one));
    assert!(html.contains(">1 file<"), "{html}");

    let none = FindPage {
        query: "nothing-here".into(),
        ..page
    };
    let html = dioxus_ssr::render_element(find(&repo, &none));
    assert!(html.contains(">0 files<"), "{html}");
    assert!(html.contains("class=\"repo-empty"), "{html}");
    assert!(html.contains("No files match"), "{html}");
    assert!(!html.contains("<tbody"), "{html}");
}
#[test]
fn go_to_file_draws_only_the_first_matches_and_says_so() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project/find/main".into(),
        ..Default::default()
    };
    let page = FindPage {
        reference: "main".into(),
        paths: (0..150)
            .map(|index| format!("src/file-{index:03}.rs"))
            .collect(),
        truncated: false,
        query: String::new(),
    };
    let html = dioxus_ssr::render_element(find(&repo, &page));
    assert!(html.contains(">150 files<"), "{html}");
    assert!(html.contains("src/file-099.rs"), "{html}");
    assert!(!html.contains("src/file-100.rs"), "{html}");
    assert!(html.contains("Showing the first 100 matches"), "{html}");
}
#[test]
fn repository_root_bar_counts_refs_and_opens_go_to_file() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project".into(),
        ..Default::default()
    };
    let page = TreePage {
        languages: Vec::new(),
        releases: None,
        reference: "main".into(),
        path: String::new(),
        branches: vec!["main".into(), "dev".into()],
        tags: vec!["v1".into()],
        entries: vec![],
        readme: None,
        latest_commit: None,
        folders: Vec::new(),
        permalink: String::new(),
        empty: false,
        http_clone: "https://example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        references: Default::default(),
    };
    let html = dioxus_ssr::render_element(tree(&repo, &page));
    assert!(html.contains("action=\"/alice/project/find/main\""));
    assert!(html.contains("Go to file"));
    assert!(html.contains("2 branches"));
    assert!(html.contains("1 tag"));
    assert!(html.contains("href=\"/alice/project/branches\""));
    let empty = TreePage {
        empty: true,
        ..page
    };
    let html = dioxus_ssr::render_element(tree(&repo, &empty));
    assert!(!html.contains("aria-label=\"Go to file\""));
}

#[test]
fn applied_deletion_does_not_claim_refusal_or_offer_a_second_delete() {
    let file = DeleteFile {
        branch: "main".into(),
        path: "old.md".into(),
        saved: true,
        error: Some("Refresh failed".into()),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(delete_file(&RepoChrome::default(), &file));
    assert!(html.contains(super::editor::SAVED_FOLLOWUP_FAILED));
    assert!(!html.contains(super::editor::DELETE_REFUSED));
    assert!(!html.contains("name=\"expected_revision\""));
    assert!(!html.contains("type=\"submit\""));
}

#[test]
fn markdown_preview_has_no_code_expansion_and_later_code_windows_have_global_anchors() {
    let mut page = BlobPage {
        reference: "main".into(),
        path: "README.md".into(),
        content: "# Heading\ntext\n".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blob(&RepoChrome::default(), &page));
    assert!(html.contains("markdown-body"));
    assert!(!html.contains("class=\"repo-code-line\""));
    page.code_view = true;
    page.offset = 300_000;
    page.first_line = Some(50_001);
    let html = dioxus_ssr::render_element(blob(&RepoChrome::default(), &page));
    assert!(html.contains("id=\"L50001\""));
    assert!(html.contains("href=\"#L50002\""));
}

#[test]
fn delete_page_names_the_file_and_commits_to_a_new_branch_when_protected() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let file = DeleteFile {
        branch: "main".into(),
        path: "docs/old.md".into(),
        expected_revision: "abc".into(),
        message: "Delete docs/old.md".into(),
        protected: true,
        new_branch: "alice-patch-1".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::editor::delete_file(&repo, &file));
    assert!(html.contains("<h2>Delete file</h2>"), "{html}");
    assert!(
        html.contains("Commit directly to the main branch"),
        "{html}"
    );
    assert!(
        html.contains("/alice/project/_forms/delete-file?from="),
        "{html}"
    );
    assert!(!html.contains("File not deleted"), "{html}");
}

#[test]
fn file_menu_copies_the_path_and_permalink_and_puts_delete_below_a_divider() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let page = BlobPage {
        reference: "main".into(),
        path: "notes.txt".into(),
        content: "hello\n".into(),
        size: 6,
        deletable: true,
        permalink: "https://example.invalid/alice/project/blob/main/notes.txt".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    let menu = html
        .split_once("class=\"native-menu repo-menu repo-more-menu\"")
        .expect("the more menu")
        .1
        .split_once("</details>")
        .expect("the end of the more menu")
        .0;
    let copy_path = menu.find("aria-label=\"Copy path\"").expect("copy path");
    let copy_permalink = menu
        .find("aria-label=\"Copy permalink\"")
        .expect("copy permalink");
    let divider = menu
        .find("class=\"repo-menu-divider\"")
        .expect("the divider");
    let delete = menu
        .find("class=\"repo-menu-item repo-menu-danger\"")
        .expect("the delete link");
    assert!(
        copy_path < copy_permalink && copy_permalink < divider,
        "{menu}"
    );
    assert!(divider < delete, "{menu}");
    // Every item draws its icon: Raw and Download (phone only), the two copy items, the delete
    // link and the "…" summary.
    assert_eq!(menu.matches("<svg").count(), 6, "{menu}");
}

#[test]
fn blob_page_offers_delete_to_writers_for_binary_files_too() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        can_write: true,
        ..Default::default()
    };
    let page = BlobPage {
        reference: "main".into(),
        path: "logo.png".into(),
        binary: true,
        deletable: true,
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(super::blob::blob(&repo, &page));
    assert!(
        html.contains("/alice/project/delete/refs/heads/main/logo.png"),
        "{html}"
    );
    assert!(!html.contains("/edit/refs/heads/main/logo.png"), "{html}");

    let reader = RepoChrome {
        can_write: false,
        ..repo
    };
    let html = dioxus_ssr::render_element(super::blob::blob(&reader, &page));
    assert!(!html.contains("/delete/refs/heads/main/logo.png"), "{html}");
}
