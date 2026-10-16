use super::*;
use super::{blob::display_text, ref_path::file_url};
use crate::webui::components::code::{self, themed_highlight};
use crate::webui::components::compact_date;

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
        next: Some(256),
        previous: None,
        truncated: true,
        can_blame: false,
        editable: true,
        deletable: true,
        code_view: false,
        references: Default::default(),
    };
    let rows = vec![RefRow {
        name: "feature/x".into(),
        sha: commit.sha.clone(),
        message: Some("Release notes".into()),
        is_default: false,
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
        new_branch: String::new(),
    };
    vec![
        Page::Tree {
            repo: repo.clone(),
            page: TreePage {
                reference: "feature/x".into(),
                path: "docs".into(),
                refs: vec!["feature/x".into(), "v1/release".into()],
                entries: vec![TreeEntry {
                    name: "ReadMe.md".into(),
                    path: "docs/ReadMe.md".into(),
                    kind: "file".into(),
                    size: Some(24),
                    last_commit: Some(commit.clone()),
                }],
                readme: Some("# README\nActual repository content".into()),
                latest_commit: Some(commit.clone()),
                empty: false,
                http_clone: "https://example.invalid/alice/project.git".into(),
                ssh_clone: "ssh://git@example.invalid/alice/project.git".into(),
                branch_count: 2,
                tag_count: 1,
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
            },
        },
        Page::Commits {
            repo: repo.clone(),
            rows: vec![commit.clone()],
            reference: "feature/x".into(),
            page: 2,
            has_more: true,
        },
        Page::CommitDetail {
            repo: repo.clone(),
            commit: commit.clone(),
            files: vec![DiffFile {
                path: "file.rs".into(),
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                patch: "-old\n+<script>alert(1)</script>".into(),
            }],
        },
        Page::Refs {
            repo: repo.clone(),
            rows: rows.clone(),
            tags: false,
            branches: rows.clone(),
        },
        Page::Refs {
            repo: repo.clone(),
            rows: rows.clone(),
            tags: true,
            branches: rows,
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
                protected: true,
                new_branch: "alice-patch-1".into(),
            },
        },
        Page::Upload {
            repo: repo.clone(),
            form: UploadForm {
                branch: "main".into(),
                directory: "docs".into(),
                expected_revision: "abc".into(),
                message: "</textarea><script>alert(1)</script>".into(),
                description: String::new(),
                error: Some("Each file can be at most 25 MiB.".into()),
                protected: false,
                new_branch: String::new(),
            },
        },
        Page::Settings {
            repo: repo.clone(),
            settings: Settings {
                tab: "labels".into(),
                collaborators: vec![Collaborator {
                    username: "bob".into(),
                    access: "write".into(),
                }],
                labels: vec![Label {
                    id: 1,
                    name: "bug".into(),
                    color: "#f85149".into(),
                    description: "A genuine label".into(),
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
                    page,
                    has_more,
                } => commits(repo, rows, reference, *page, *has_more),
                Page::CommitDetail {
                    repo,
                    commit,
                    files,
                } => commit_detail(repo, commit, files),
                Page::Refs {
                    repo,
                    rows,
                    tags,
                    branches,
                } => refs(repo, rows, *tags, branches),
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
    assert!(!tree_html.contains("href=\"/alice/project/tree/feature/x/docs/ReadMe.md\""));
    assert!(tree_html.contains("href=\"/alice/project/blob/feature/x/docs/ReadMe.md\""));
    assert!(tree_html.contains("/api/repos/alice/project/archive?ref=feature%2Fx"));
    let blob_html = dioxus_ssr::render_element(render(pages[1].clone()));
    assert!(blob_html.contains("Copy preview"));
    assert!(blob_html.contains("href=\"/alice/project/raw/feature/x/a%20%23%3F.rs\""));
    assert!(blob_html.contains("href=\"/alice/project/edit/feature/x/a%20%23%3F.rs\""));
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
        for element in [
            editor(&repo, &state),
            settings(&repo, &Settings::default()),
            refs(&repo, &[], true, &[]),
            rejected_form(&repo, "create-tag", "Review this draft", &fields),
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
            assert_eq!(count, 1);
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
    let html = dioxus_ssr::render_element(super::editor(&repo, &editor));
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
        dioxus_ssr::render_element(super::editor(&repo, &editor))
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
    let render = |editor: Editor| dioxus_ssr::render_element(super::editor(&repo, &editor));
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
    let html = dioxus_ssr::render_element(super::editor(&repo, &editor));
    // The page's stylesheet comes first in the markup, so search for the elements themselves.
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
    let html = dioxus_ssr::render_element(super::editor(&repo, &editor));
    assert!(html.contains("repo-code-editor-gutter"));
    assert!(html.contains(">1\n2\n3</pre>"), "{html}");
    assert!(html.contains("3 lines"));
    // A short file still gets a field tall enough to work in.
    assert!(html.contains("rows=20"), "{html}");
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
    let html = dioxus_ssr::render_element(super::editor(&repo, &editor));
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
    let preview = rendered(super::editor(&repo, &editor));
    assert_eq!(preview, rendered(super::blob(&repo, &page)));
    assert!(preview.contains(">a b</p>"));
    assert!(preview.contains("<sup class=\"footnote-reference\">"));
    assert!(preview.contains("href=\"/alice/project/blob/main/docs/guide.md#user-content-top\""));
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
    }];
    let refs_html = dioxus_ssr::render_element(refs(&repo, &rows, false, &rows));
    assert!(refs_html.contains("/_forms/delete-branch"));
    assert!(refs_html.contains("name=\"confirm\""));
    assert!(refs_html.contains("<details"));
    assert!(!refs_html.contains("onclick="));
    assert!(refs_html.contains("repo-ref-list-panel"));
    assert!(refs_html.contains("repo-ref-create"));
}

/// The settings page as it renders on one section.
fn settings_tab_html(repo: &RepoChrome, tab: &str) -> String {
    dioxus_ssr::render_element(settings(
        repo,
        &Settings {
            tab: tab.into(),
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
        ("labels", "_forms/label\""),
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
        ("labels", "Labels"),
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
    let labels = settings_tab_html(&repo, "labels");
    assert!(labels.contains("repo-color-swatch"), "{labels}");
    assert!(labels.contains("background-color: #58a6ff"), "{labels}");
}

#[test]
fn label_rows_show_a_colour_dot_only_for_a_valid_colour() {
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
    let label = |color: &str| Label {
        id: 4,
        name: "bug".into(),
        color: color.into(),
        description: String::new(),
    };
    let html = dioxus_ssr::render_element(settings(
        &repo,
        &Settings {
            tab: "labels".into(),
            labels: vec![label("d73a4a")],
            ..Settings::default()
        },
    ));
    let row = html
        .split_once("class=\"repo-label-preview\"")
        .unwrap_or_else(|| panic!("label row: {html}"))
        .1;
    assert!(row.contains("repo-label-dot"), "{row}");
    assert!(row.contains("background-color: #d73a4a"), "{row}");
    // A stored colour that is not six hex digits never reaches a style attribute.
    let html = dioxus_ssr::render_element(settings(
        &repo,
        &Settings {
            tab: "labels".into(),
            labels: vec![label("url(http://x.test)")],
            ..Settings::default()
        },
    ));
    assert!(html.contains("repo-label-dot"), "{html}");
    assert!(!html.contains("background-color: url"), "{html}");
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
    };
    let branches = [row("main", true), row("feature/x", false)];
    let html = dioxus_ssr::render_element(refs(&repo, &branches, false, &branches));
    assert!(html.contains("href=\"/alice/project/compare/main...feature/x\""));
    assert!(!html.contains("compare/main...main"));
    let tags = dioxus_ssr::render_element(refs(&repo, &[row("v1", false)], true, &branches));
    assert!(!tags.contains("/compare/"));
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
    };
    let branches = dioxus_ssr::render_element(refs(
        &repo,
        &[row("main"), row("dev")],
        false,
        &[row("main")],
    ));
    assert!(branches.contains("2 branches"));
    assert!(branches.contains("title-with-count"));
    assert!(!branches.contains(" total<"));
    assert!(branches.contains("btn btn-danger btn-sm"));
    assert!(!branches.contains("btn-destructive"));
    let tags = dioxus_ssr::render_element(refs(&repo, &[row("v1")], true, &[row("main")]));
    assert!(tags.contains("1 tag<"));
    // The danger zone keeps the filled button.
    let settings_html = settings_tab_html(&repo, "danger");
    assert!(settings_html.contains("btn btn-destructive"));
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
    let html = dioxus_ssr::render_element(super::settings(&repo, &settings));
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
    let html = dioxus_ssr::render_element(super::settings(&repo, &refused));
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
    let html = dioxus_ssr::render_element(super::settings(&repo, &settings));
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
        force_push_suggestions: vec![person("bob", "write"), person("carol", "admin")],
        branch_rules: vec![BranchRule {
            pattern: "main".into(),
            allow_force_pushes: false,
            force_push_allowances: vec!["bob".into(), "dave".into()],
            ..BranchRule::default()
        }],
        ..Settings::default()
    };
    let html = dioxus_ssr::render_element(super::settings(&repo, &settings));
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
    let html = dioxus_ssr::render_element(super::settings(&repo, &settings));
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
    let html = dioxus_ssr::render_element(super::settings(&repo, &refused));
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
    let html = dioxus_ssr::render_element(super::editor(&repo, &saved));
    assert!(html.contains("File saved; follow-up failed"));
    assert!(!html.contains("Changes were not saved"));
    let failed = Editor {
        saved: false,
        ..saved
    };
    let html = dioxus_ssr::render_element(super::editor(&repo, &failed));
    assert!(html.contains("Changes were not saved"));
    assert!(!html.contains("File saved"));
}
#[test]
fn editor_wraps_long_lines_on_phones_and_keeps_desktop_scrolling() {
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
    // The first phone-width block holds the wrapping rule, and the highlight layer wraps with it.
    let phone = css.split_once("@media (max-width: 639px) {").unwrap().1;
    assert!(
        phone.contains("textarea.repo-editor-textarea, .repo-editor-highlight { white-space: pre-wrap; overflow-wrap: anywhere; }"),
        "{phone}"
    );
}

#[test]
fn settings_and_ref_confirmations_clear_the_header_and_stay_in_flow() {
    let css = include_str!("../../components/native.css");
    // Anchors and the sticky settings nav must clear the sticky site header.
    assert!(css.contains("--site-header-height: 52px;"));
    assert!(css.contains("top: calc(var(--site-header-height) + 16px);"));
    assert!(css.contains("scroll-margin-top: calc(var(--site-header-height) + 16px);"));
    // The ref delete confirmation is in normal flow; an absolute popover is clipped by its panel.
    let popover = css
        .split_once(".repo-confirm-popover {")
        .unwrap()
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(!popover.contains("absolute"));
    assert!(!popover.contains("z-index"));
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
    assert!(settings_tab_html(&repo, "labels").contains("/_forms/label\""));
}
#[test]
fn protocol_links_use_existing_archive_and_slash_ref_raw_adapter() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        ..Default::default()
    };
    let page = TreePage {
        reference: "feature/x".into(),
        path: String::new(),
        refs: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        empty: false,
        http_clone: "http://example.invalid".into(),
        ssh_clone: "ssh://git@example.invalid".into(),
        branch_count: 0,
        tag_count: 0,
        references: Default::default(),
    };
    let html = dioxus_ssr::render_element(tree(&repo, &page));
    assert!(html.contains("/api/repos/alice/project/archive?ref=feature%2Fx"));
    assert!(!html.contains("href=\"/alice/project/archive"));
}
#[test]
fn empty_repository_quick_setup_is_shown_only_to_writers() {
    let page = TreePage {
        reference: "main".into(),
        path: String::new(),
        refs: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        empty: true,
        http_clone: "https://git.example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        branch_count: 0,
        tag_count: 0,
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
    let html = dioxus_ssr::render_element(tree(&chrome(true), &page));
    assert!(html.contains("This repository is empty"));
    assert!(html.contains("Create new file"));
    assert!(html.contains("Quick setup"));
    assert!(html.contains("Create a new repository on the command line"));
    assert!(html.contains("Push an existing repository from the command line"));
    assert!(html.contains("<pre class=\"repo-quick-setup-code\">"));
    assert!(html.contains("git init"));
    assert!(html.contains("git add README.md"));
    assert!(html.contains("git branch -M main"));
    assert!(html.contains("git remote add origin https://git.example.invalid/alice/project.git"));
    assert!(html.contains("git push -u origin main"));

    let reader = dioxus_ssr::render_element(tree(&chrome(false), &page));
    assert!(reader.contains("This repository is empty"));
    assert!(!reader.contains("Create new file"));
    assert!(!reader.contains("Quick setup"));
    assert!(!reader.contains("git init"));
}

#[test]
fn prerendered_static_repository_css_has_no_raw_text_markers() {
    crate::webui::tests::in_scope(|| {
        let mut renderer = dioxus_ssr::Renderer::new();
        renderer.pre_render = true;
        let html = renderer.render_element(render(fixtures()[0].clone()));
        let css = html
            .split_once("<style")
            .unwrap()
            .1
            .split_once('>')
            .unwrap()
            .1
            .split_once("</style>")
            .unwrap()
            .0;
        assert_eq!(css, include_str!("style.css"));
        assert!(!css.contains("<!--"));
        assert!(!css.contains("&gt;"));
    });
}

#[test]
fn restored_file_browser_integrates_commit_rows_and_real_sidebar_data() {
    crate::webui::tests::in_scope(|| {
        let Page::Tree { mut repo, mut page } = fixtures().remove(0) else {
            unreachable!()
        };
        page.path.clear();
        repo.stars = 7;
        repo.watchers = 3;
        repo.forks = 2;
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
            table.find("/tree/feature/x/src").unwrap()
                < table.find("/blob/feature/x/docs/ReadMe.md").unwrap()
        );
        assert!(
            table.find("/blob/feature/x/docs/ReadMe.md").unwrap()
                < table.find("/blob/feature/x/z-last.txt").unwrap()
        );
        assert!(table.contains("repo-sr-only"));
        assert!(table.contains("Oct 2, 2026"));
        assert!(table.contains("datetime=\"2026-10-02T00:00:00Z\""));
        assert!(!table.contains("▸"));
        assert!(html.contains("repo-code-button"));
        assert!(html.contains("Create new file"));
        assert!(html.contains("Download ZIP"));
        assert!(html.contains("repo-readme-header"));
        let sidebar = html.split_once("<aside").unwrap().1;
        assert!(sidebar.contains("An actual description"));
        assert!(sidebar.contains("<strong>7</strong>"));
        assert!(sidebar.contains("<strong>3</strong>"));
        assert!(sidebar.contains("<strong>2</strong>"));
        assert!(!sidebar.contains("Releases"));
        assert!(!sidebar.contains("Languages"));
        page.path = "docs".into();
        let subtree = dioxus_ssr::render_element(tree(&repo, &page));
        // A directory has the same layout and About sidebar as the root.
        assert!(subtree.contains("class=\"repo-tree-layout\""));
        assert!(subtree.contains("aria-label=\"File path\""));
        let sidebar = subtree.split_once("<aside").unwrap().1;
        assert!(sidebar.contains("An actual description"));
        assert!(sidebar.contains("<strong>7</strong>"));
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
fn restored_chrome_keeps_native_engagement_and_fork_denials() {
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
        let html = dioxus_ssr::render_element(chrome(&repo, rsx! {}));
        assert!(html.contains("repo-visibility"));
        assert!(html.contains("repo-engagement-count"));
        let settings_link = html
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
        for action in ["watch", "star", "fork"] {
            assert!(html.contains(&format!("action=\"/alice/project/_forms/{action}\"")));
        }
        assert_eq!(
            html.matches("<form ").count(),
            html.matches("name=\"csrf_token\"").count()
        );
        // A reader may fork a private repository; the fork keeps it private.
        repo.is_private = true;
        let private = dioxus_ssr::render_element(chrome(&repo, rsx! {}));
        assert!(private.contains("action=\"/alice/project/_forms/fork\""));
        repo.is_private = false;
        repo.fork_refusal = Some("Cannot fork your own repository".into());
        let own = dioxus_ssr::render_element(chrome(&repo, rsx! {}));
        assert!(!own.contains("/_forms/fork"));
        assert!(own.contains("title=\"Cannot fork your own repository\""));
        repo.fork_refusal = Some("Archived repositories cannot be forked".into());
        let archived = dioxus_ssr::render_element(chrome(&repo, rsx! {}));
        assert!(!archived.contains("/_forms/fork"));
        assert!(archived.contains("title=\"Archived repositories cannot be forked\""));
        repo.fork_refusal = None;
        repo.signed_in = false;
        repo.can_admin = false;
        let anonymous = dioxus_ssr::render_element(chrome(&repo, rsx! {}));
        assert!(!anonymous.contains("<form "));
        assert!(!anonymous.contains("/project/settings"));
        assert!(anonymous.contains("/login?redirect=%2Falice%2Fproject"));
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
    assert!(preview.contains("href=\"/alice/project/blob/main/docs/guide.md?view=code\""));
    assert!(preview.contains("href=\"/alice/project/raw/main/docs/guide.md\""));
    assert!(!preview.contains("Copy code"));
    assert!(!preview.contains("<pre class=\"repo-line-numbers\""));

    page.code_view = true;
    let code = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(code.contains("Copy code"));
    assert!(code.contains("<pre class=\"repo-line-numbers\""));
    assert!(!code.contains("<div class=\"repo-readme-body\">"));
    assert!(code.contains("href=\"/alice/project/blob/main/docs/guide.md\""));
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
    let file = DiffFile {
        path: "notes.txt".into(),
        status: "added".into(),
        additions: 1,
        deletions: 0,
        patch: "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n".into(),
    };
    let html = dioxus_ssr::render_element(commit_detail(&repo, &commit, &[file]));
    assert!(html.contains("class=\"patch-badge\""));
    assert!(html.contains("New file"));
    assert!(html.contains("<span>1 file</span>"));
    assert!(html.contains("+hello"));
    assert!(!html.contains("diff --git"));
    assert!(!html.contains("new file mode"));
    assert!(!html.contains("--- /dev/null"));
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
        },
        RefRow {
            name: "topic".into(),
            sha: topic.clone(),
            message: None,
            is_default: false,
        },
    ];
    let html = dioxus_ssr::render_element(refs(&repo, &branches, false, &branches));
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
    assert!(html.contains("href=\"/alice/project/blob/main/docs/a.txt\""));
    assert!(html.contains("href=\"/alice/project/raw/main/docs/a.txt\""));
    // Source stays escaped, and the carriage return does not make a line of its own.
    assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"));
    assert!(!html.contains("<script>x"));
    assert!(html.contains("TWO"));
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
    assert!(html.contains("href=\"/alice/project/blame/main/docs/guide.txt\""));
    page.can_blame = false;
    let html = dioxus_ssr::render_element(blob(&repo, &page));
    assert!(!html.contains("/blame/"));
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
        reference: "main".into(),
        path: "docs".into(),
        refs: vec![],
        entries: vec![],
        readme: None,
        latest_commit: None,
        empty: false,
        http_clone: "https://git.example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        branch_count: 0,
        tag_count: 0,
        references: Default::default(),
    };
    let writer = dioxus_ssr::render_element(tree(&chrome(true), &directory));
    assert!(writer.contains("Upload files"));
    assert!(writer.contains("href=\"/alice/project/upload/main/docs\""));
    let reader = dioxus_ssr::render_element(tree(&chrome(false), &directory));
    assert!(!reader.contains("/upload/"));
    let empty = TreePage {
        empty: true,
        ..directory
    };
    let writer = dioxus_ssr::render_element(tree(&chrome(true), &empty));
    assert!(writer.contains("href=\"/alice/project/upload/main/docs\""));
    let reader = dioxus_ssr::render_element(tree(&chrome(false), &empty));
    assert!(!reader.contains("/upload/"));
}
#[test]
fn path_filter_needs_every_term_and_ignores_case() {
    use super::find::path_matches;
    assert!(path_matches("docs/Guide.md", ""));
    assert!(path_matches("docs/Guide.md", "  guide   DOCS "));
    assert!(path_matches("src/lib.rs", "lib.rs"));
    assert!(!path_matches("docs/Guide.md", "guide readme"));
    assert!(!path_matches("src/lib.rs", "lib.rs.bak"));
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
    assert!(html.contains("href=\"/alice/project/blob/main/docs/guide.md\""));
    assert!(!html.contains("src/lib.rs"));
    assert!(html.contains("class=\"repo-find-first\""));
    assert!(html.contains("Showing the first 10,000 files."));
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
fn repository_root_bar_counts_refs_and_opens_go_to_file() {
    let repo = RepoChrome {
        owner: "alice".into(),
        name: "project".into(),
        current_path: "/alice/project".into(),
        ..Default::default()
    };
    let page = TreePage {
        reference: "main".into(),
        path: String::new(),
        refs: vec!["main".into()],
        branch_count: 2,
        tag_count: 1,
        entries: vec![],
        readme: None,
        latest_commit: None,
        empty: false,
        http_clone: "https://example.invalid/alice/project.git".into(),
        ssh_clone: String::new(),
        references: Default::default(),
    };
    let html = dioxus_ssr::render_element(tree(&repo, &page));
    assert!(html.contains("href=\"/alice/project/find/main\""));
    assert!(html.contains("Go to file"));
    assert!(html.contains("2 branches"));
    assert!(html.contains("1 tag"));
    assert!(html.contains("href=\"/alice/project/branches\""));
    let empty = TreePage {
        empty: true,
        ..page
    };
    let html = dioxus_ssr::render_element(tree(&repo, &empty));
    assert!(!html.contains("Go to file"));
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
    let html = dioxus_ssr::render_element(super::delete_file(&repo, &file));
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
    let html = dioxus_ssr::render_element(super::blob(&repo, &page));
    assert!(
        html.contains("/alice/project/delete/main/logo.png"),
        "{html}"
    );
    assert!(!html.contains("/edit/main/logo.png"), "{html}");

    let reader = RepoChrome {
        can_write: false,
        ..repo
    };
    let html = dioxus_ssr::render_element(super::blob(&reader, &page));
    assert!(!html.contains("/delete/main/logo.png"), "{html}");
}
