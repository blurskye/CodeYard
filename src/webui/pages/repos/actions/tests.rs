use super::dispatch::input_field;
use super::*;
use super::{logs::LogLine, run::workflow_file_url, runs::repository_base};
use crate::webui::pages::ci::{render, Page};
use crate::webui::pages::notifications::NotificationsPage;
use crate::webui::pages::repos::chrome::RepoChrome;
use dioxus::prelude::*;
use std::collections::BTreeMap;
fn run() -> Run {
    Run {
        id: "real-run".into(),
        repository: "repo".into(),
        status: "running".into(),
        workflow_path: ".github/workflows/ci.yml".into(),
        commit_status_context: "gitserv/ci".into(),
        branch: "feature/slash".into(),
        event: "push".into(),
        pipeline: "ci".into(),
        mode: "act".into(),
        head_sha: None,
        created_at: 1,
        started_at: None,
        completed_at: None,
        error_message: None,
        inputs: Default::default(),
    }
}
fn repo_chrome() -> RepoChrome {
    RepoChrome {
        id: 1,
        owner: "alice".into(),
        name: "repo".into(),
        default_branch: "main".into(),
        description: "Actions fixture repository".into(),
        website: String::new(),
        topics: Vec::new(),
        is_private: false,
        is_archived: false,
        can_write: true,
        can_admin: true,
        is_owner: false,
        can_manage_collaborators: false,
        fork_refusal: None,
        signed_in: true,
        viewer_id: 1,
        starred: true,
        watch_level: "watching".to_owned(),
        watch_events: String::new(),
        stars: 7,
        open_issues: 0,
        open_pulls: 0,
        watchers: 3,
        forks: 2,
        fork_url: None,
        parent_name: None,
        parent_url: None,
        csrf_token: "test-token".into(),
        current_path: "/alice/repo/actions".into(),
    }
}
fn model() -> ActionsPage {
    ActionsPage {
        repository: Some(("alice".into(), "repo".into())),
        repo_chrome: Some(repo_chrome()),
        title: "Actions".into(),
        base_path: "/alice/repo/actions".into(),
        csrf_token: "test-token".into(),
        query: BTreeMap::new(),
        runs: vec![run()],
        workflows: vec![Workflow {
            path: ".github/workflows/ci.yml".into(),
            name: "CI".into(),
            dispatchable: true,
            inputs: Vec::new(),
        }],
        selected: None,
        dispatch_branch: "main".into(),
        can_dispatch: true,
        run_limit: 100,
        log_limit: 2000,
    }
}
#[test]
fn filters_are_conjunctive_and_urls_preserve_selection() {
    let mut query = BTreeMap::from([
        ("branch".into(), "feature/slash".into()),
        ("status".into(), "running".into()),
        ("log_q".into(), "a & b".into()),
    ]);
    assert!(matches_filters(&run(), &query));
    query.insert("event".into(), "release".into());
    assert!(!matches_filters(&run(), &query));
    let url = query_url("/ci", &query, &[("job", "job-id"), ("wrap", "0")]);
    assert!(url.contains("branch=feature%2Fslash"));
    assert!(url.contains("log_q=a%20%26%20b"));
    assert!(url.contains("job=job-id"));
}
#[test]
fn ssr_has_real_navigation_and_csrf_dispatch_form() {
    let mut dom = VirtualDom::new_with_props(|props: ActionsPage| actions_page(props), model());
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("real-run"));
    assert!(html.contains("method=\"post\""));
    assert!(html.contains("/alice/repo/actions/dispatch"));
    assert!(html.contains("csrf_token"));
    assert!(!html.contains("<script")); // Initial view/fallback only, not a global runtime ban.
}
#[test]
fn persisted_output_is_escaped_and_absent_deployment_is_honest() {
    let mut model = model();
    model.selected = Some(SelectedRun {
        run: run(),
        logs: vec![LogLine {
            id: 42,
            stage: "ci".into(),
            line: "<script>unsafe & output</script>".into(),
            level: "error".into(),
            timestamp: 1,
        }],
        deployment: None,
        fetched_lines: 1,
        logs_truncated: false,
    });
    let mut dom = VirtualDom::new_with_props(|props: ActionsPage| actions_page(props), model);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("&#60;script&#62;unsafe &#38; output&#60;/script&#62;"));
    assert!(!html.contains("actions-deployment-card"));
    assert!(!html.contains("No deployment is recorded"));
    assert!(html.contains("actions-annotations"));
    assert!(html.contains("actions-log-line is-error"));
    assert!(html.contains("log-42"));
    assert!(html.contains(" open=true") || html.contains(" open=\"true\""));
    assert!(!html.contains("<script>unsafe"));
}

#[test]
fn only_a_failed_run_shows_the_error_banner() {
    let mut model = model();
    let mut cancelled = run();
    cancelled.status = "cancelled".into();
    cancelled.error_message = Some("Cancelled".into());
    model.selected = Some(SelectedRun {
        run: cancelled,
        logs: Vec::new(),
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    });
    assert!(!html(model.clone()).contains("This run reported an error"));
    let mut failed = run();
    failed.status = "failed".into();
    failed.error_message = Some("A workflow job failed".into());
    model.selected.as_mut().unwrap().run = failed;
    assert!(html(model).contains("This run reported an error"));
}

fn html(model: ActionsPage) -> String {
    let mut dom = VirtualDom::new_with_props(|props: ActionsPage| actions_page(props), model);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

/// The repository tab row the header draws for the fixture repository on Actions pages.
fn repo_tabs_html() -> String {
    dioxus_ssr::render_element(crate::webui::pages::repos::chrome::repo_tabs(&repo_chrome()))
}

/// The site header band for a repository page: the name, its badges, the engagement buttons and
/// the tabs. The page body does not draw any of them.
fn repo_header_html(repo: &RepoChrome) -> String {
    dioxus_ssr::render_element(crate::webui::components::shell(
        crate::webui::document::Chrome::default(),
        Some(repo.clone()),
        rsx! {},
    ))
}

fn assert_actions_tab_active(tabs: &str, base: &str) {
    let target = format!("href=\"{base}/actions\"");
    let attributes = tabs
        .split("<a ")
        .filter_map(|link| link.split_once('>').map(|(attributes, _)| attributes))
        .find(|attributes| attributes.contains(&target))
        .expect("shared Actions tab");
    assert!(attributes.contains("class=\"repo-tab repo-tab-active\""));
    assert_eq!(
        tabs.matches("class=\"repo-tab repo-tab-active\"").count(),
        1
    );
}

#[test]
fn repository_list_uses_shared_chrome_and_preserves_original_actions_body() {
    let rendered = html(model());
    assert_eq!(rendered.matches("<h1").count(), 1); // The Actions heading; the name heads the band.
    assert!(rendered.contains("All workflows"));
    assert_eq!(rendered.matches("class=\"repo-content ").count(), 1);
    assert!(!rendered.contains("actions-repository-tab"));
    // The site header band draws the tabs, the name, the badges and the engagement buttons, so
    // they are not in the page body.
    let tabs = repo_tabs_html();
    let header = repo_header_html(&repo_chrome());
    assert_actions_tab_active(&tabs, "/alice/repo");
    for href in [
        "/alice/repo",
        "/alice/repo/issues",
        "/alice/repo/pulls",
        "/alice/repo/actions",
        "/alice/repo/commits",
        "/alice/repo/branches",
        "/alice/repo/tags",
        "/alice/repo/settings",
    ] {
        assert!(tabs.contains(&format!("href=\"{href}\"")), "{href}");
    }
    for action in ["watch-level", "unstar", "fork"] {
        assert!(header.contains(&format!("action=\"/alice/repo/_forms/{action}\"")));
    }
    for count in [7, 3, 2] {
        assert!(header.contains(&format!("class=\"repo-engagement-count\">{count}</span>")));
    }
    assert!(header.contains("repo-visibility-text\">Public</span>"));
    // The header's own row is not in the page body, and neither is a phone row of the buttons.
    assert!(!rendered.contains("class=\"repo-engagement flex"));
    assert!(!rendered.contains("repo-engagement-phone"));
    assert!(rendered.contains("actions-list-layout"));
    assert!(rendered.contains("actions-content"));
    assert!(rendered.contains("actions-run-toolbar"));
    assert!(rendered.contains("<ul>"));
    assert!(rendered.contains("<li>"));
    assert!(rendered.contains("actions-run-row"));
    assert!(rendered.contains("Run ID real-run"));
    assert!(rendered.contains("id=\"actions-run-filters\""));
    assert!(rendered.contains("name=\"q\""));
}

#[test]
fn repository_chrome_preserves_authorized_visibility_and_settings_controls() {
    let mut repo = repo_chrome();
    repo.is_private = true;
    repo.is_archived = true;
    repo.can_admin = false;
    repo.fork_refusal = Some("Archived repositories cannot be forked".into());
    let header = repo_header_html(&repo);
    assert!(header.contains("repo-visibility-text\">Private</span>"));
    assert!(header.contains("repo-visibility-text\">Archived</span>"));
    let tabs = dioxus_ssr::render_element(crate::webui::pages::repos::chrome::repo_tabs(&repo));
    assert!(!tabs.contains("href=\"/alice/repo/settings\""));
    assert!(!header.contains("action=\"/alice/repo/_forms/fork\""));
    assert!(header.contains("Archived repositories cannot be forked"));
    for action in ["watch-level", "unstar"] {
        assert!(header.contains(&format!("action=\"/alice/repo/_forms/{action}\"")));
    }
    assert_eq!(
        header.matches("<form method=\"post\"").count(),
        header.matches("name=\"csrf_token\"").count()
    );
}

#[test]
fn global_actions_and_notifications_have_no_repository_chrome() {
    let mut global = model();
    global.repository = None;
    global.repo_chrome = None;
    global.base_path = "/ci".into();
    let rendered = html(global);
    assert!(!rendered.contains("class=\"repo-content "));
    assert!(!rendered.contains("actions-repository-tab"));
    assert!(!rendered.contains("/alice/repo/_forms/"));
    assert!(rendered.contains("/ci/dispatch"));
    let mut dom = VirtualDom::new_with_props(
        render,
        Page::Notifications(NotificationsPage {
            csrf_token: "test-token".into(),
            notifications: vec![],
            unread_count: 0,
            view: "inbox".into(),
            counts: Default::default(),
            repository: None,
            repositories: vec![],
            reason: None,
            reasons: vec![],
            query: None,
            unread_only: false,
            before: None,
            older: None,
        }),
    );
    dom.rebuild_in_place();
    let notifications = dioxus_ssr::render(&dom);
    assert!(!notifications.contains("class=\"repo-content "));
    assert!(!notifications.contains("/alice/repo"));
}

#[test]
fn repository_identity_and_path_alone_never_fabricate_chrome() {
    let mut page = model();
    page.repo_chrome = None;
    let rendered = html(page);
    assert!(!rendered.contains("class=\"repo-content "));
    assert!(!rendered.contains("actions-repository-tab"));
    assert!(!rendered.contains("/alice/repo/_forms/"));
}

#[test]
fn native_dispatch_recovery_uses_authorized_chrome_and_escapes_draft_fields() {
    let mut repo = repo_chrome();
    repo.current_path = "/alice/repo/actions/dispatch".into();
    let draft = DispatchDraft {
        owner: "alice".into(),
        repository: "repo".into(),
        branch: "feature/\"<draft>&".into(),
        workflow: ".github/workflows/\"<workflow>&.yml".into(),
        ..DispatchDraft::default()
    };
    let recovery = Page::DispatchRecovery {
        repo_chrome: Some(Box::new(repo)),
        base_path: "/alice/repo/actions".into(),
        csrf_token: "<csrf>&".into(),
        message: "<script>rejected & unsafe</script>".into(),
        draft: Box::new(draft.clone()),
        workflows: vec![Workflow {
            path: draft.workflow.clone(),
            name: "Workflow".into(),
            dispatchable: true,
            inputs: Vec::new(),
        }],
    };
    let mut dom = VirtualDom::new_with_props(render, recovery.clone());
    dom.rebuild_in_place();
    let rendered = dioxus_ssr::render(&dom);
    assert!(rendered.contains("class=\"repo-content "));
    assert_actions_tab_active(&repo_tabs_html(), "/alice/repo");
    assert!(!rendered.contains("actions-repository-tab"));
    assert!(rendered.contains("action=\"/alice/repo/actions/dispatch\""));
    assert!(rendered.contains("feature/&#34;&#60;draft&#62;&#38;"));
    assert!(rendered.contains("&#60;workflow&#62;&#38;.yml"));
    assert!(rendered.contains("&#60;csrf&#62;&#38;"));
    assert!(rendered.contains("&#60;script&#62;rejected &#38; unsafe&#60;/script&#62;"));
    assert!(!rendered.contains("<script>rejected"));
    let decoded: Page = serde_json::from_str(&serde_json::to_string(&recovery).unwrap()).unwrap();
    assert_eq!(decoded, recovery);
    let global = Page::DispatchRecovery {
        repo_chrome: None,
        base_path: "/ci".into(),
        csrf_token: "test-token".into(),
        message: "Run was not accepted".into(),
        draft: Box::new(draft),
        workflows: Vec::new(),
    };
    let mut dom = VirtualDom::new_with_props(render, global);
    dom.rebuild_in_place();
    let rendered = dioxus_ssr::render(&dom);
    assert!(!rendered.contains("class=\"repo-content "));
    assert!(!rendered.contains("/alice/repo/_forms/"));
    assert!(rendered.contains("action=\"/ci/dispatch\""));
}

#[test]
fn native_repository_chrome_escapes_identity_and_encodes_real_paths() {
    let mut page = model();
    let repo = page.repo_chrome.as_mut().unwrap();
    repo.owner = "<owner &>".into();
    repo.name = "repo\"<name>".into();
    repo.current_path = format!("{}/actions", repo.base());
    let base = repo.base();
    page.repository = Some((repo.owner.clone(), repo.name.clone()));
    page.base_path = format!("{base}/actions");
    let tabs = dioxus_ssr::render_element(crate::webui::pages::repos::chrome::repo_tabs(
        page.repo_chrome.as_ref().unwrap(),
    ));
    let header = repo_header_html(page.repo_chrome.as_ref().unwrap());
    let rendered = html(page);
    assert!(header.contains("&#60;owner &#38;&#62;"));
    assert!(header.contains("repo&#34;&#60;name&#62;"));
    assert!(header.contains(&format!("action=\"{base}/_forms/watch-level\"")));
    assert!(rendered.contains(&format!("action=\"{base}/actions/dispatch\"")));
    assert_actions_tab_active(&tabs, &base);
    assert!(!rendered.contains("<owner &>"));
    assert!(!rendered.contains("repo\"<name>"));
}

#[test]
fn filters_and_dispatch_keep_native_fields_query_context_and_authorized_options() {
    let mut page = model();
    page.query = BTreeMap::from([
        ("ref".into(), "feature/slash".into()),
        ("branch".into(), "feature/slash".into()),
        ("q".into(), "real-run".into()),
        ("workflow".into(), ".github/workflows/ci.yml".into()),
    ]);
    let rendered = html(page.clone());
    for name in [
        "q",
        "workflow",
        "event",
        "status",
        "branch",
        "ref",
        "csrf_token",
        "return_query",
        "workflow_path",
        "environment",
    ] {
        assert!(rendered.contains(&format!("name=\"{name}\"")), "{name}");
    }
    assert!(rendered.contains("feature%2Fslash"));
    assert!(rendered.contains("test-token"));
    assert!(rendered.contains("actions-dispatch-menu"));
    page.can_dispatch = false;
    assert!(!html(page.clone()).contains("action=\"/alice/repo/actions/dispatch\""));
    page.can_dispatch = true;
    page.workflows[0].dispatchable = false;
    let rendered = html(page);
    assert!(!rendered.contains("Run workflow"));
    assert!(!rendered.contains("action=\"/alice/repo/actions/dispatch\""));
}

#[test]
fn dispatch_form_renders_one_named_field_per_declared_input() {
    use super::{InputKind, WorkflowInput};
    let mut page = model();
    page.workflows[0].inputs = vec![
        WorkflowInput {
            name: "target".into(),
            description: "Where to deploy".into(),
            kind: InputKind::Choice,
            required: true,
            default: None,
            options: vec!["staging".into(), "production".into()],
        },
        WorkflowInput {
            name: "debug".into(),
            description: String::new(),
            kind: InputKind::Boolean,
            required: false,
            default: Some("true".into()),
            options: Vec::new(),
        },
        WorkflowInput {
            name: "retries".into(),
            description: String::new(),
            kind: InputKind::Number,
            required: false,
            default: Some("2".into()),
            options: Vec::new(),
        },
    ];
    let rendered = html(page);
    for name in ["target", "debug", "retries"] {
        assert!(
            rendered.contains(&format!("name=\"input:.github/workflows/ci.yml:{name}\"")),
            "{name}"
        );
    }
    // A checkbox sends nothing when unchecked, so the hidden false is what the server reads then.
    assert!(rendered.contains("type=\"checkbox\""));
    assert!(rendered.contains("value=\"false\""));
    assert!(rendered.contains("Where to deploy"));
}

#[test]
fn run_page_lists_the_inputs_a_manual_run_was_started_with() {
    let mut model = model();
    let mut run = run();
    run.inputs = BTreeMap::from([("target".to_owned(), "production".to_owned())]);
    model.selected = Some(SelectedRun {
        run,
        logs: Vec::new(),
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    });
    let rendered = html(model);
    assert!(rendered.contains("Input target"));
    assert!(rendered.contains("production"));
}

/// The start tag holding `name="<name>"`, so a test can see which attributes that field has.
fn tag_named(rendered: &str, name: &str) -> String {
    let at = rendered
        .find(&format!("name=\"{name}\""))
        .expect("field is rendered");
    let start = rendered[..at].rfind('<').expect("start tag");
    let end = at + rendered[at..].find('>').expect("end of tag");
    rendered[start..=end].to_owned()
}

#[test]
fn required_inputs_are_html_required_only_on_a_single_workflow_form() {
    let target = WorkflowInput {
        name: "target".into(),
        description: String::new(),
        kind: InputKind::Text,
        required: true,
        default: None,
        options: Vec::new(),
    };
    let render_field = |html_required: bool| {
        let mut dom = VirtualDom::new_with_props(
            |props: (WorkflowInput, bool)| {
                let (input, html_required) = props;
                rsx! { form { {input_field(".github/workflows/ci.yml", &input, &BTreeMap::new(), html_required)} } }
            },
            (target.clone(), html_required),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    };
    let name = "input:.github/workflows/ci.yml:target";
    let single = render_field(true);
    assert!(tag_named(&single, name).contains("required"), "{single}");
    assert!(!single.contains("(required)"), "{single}");
    let several = render_field(false);
    assert!(!tag_named(&several, name).contains("required"), "{several}");
    assert!(several.contains("(required)"), "{several}");
}

#[test]
fn refused_repository_run_keeps_the_typed_inputs_and_names_the_reason() {
    let workflow = Workflow {
        path: ".github/workflows/deploy.yml".into(),
        name: "Deploy".into(),
        dispatchable: true,
        inputs: vec![
            WorkflowInput {
                name: "target".into(),
                description: String::new(),
                kind: InputKind::Choice,
                required: true,
                default: None,
                options: vec!["staging".into(), "production".into()],
            },
            WorkflowInput {
                name: "retries".into(),
                description: String::new(),
                kind: InputKind::Number,
                required: false,
                default: Some("2".into()),
                options: Vec::new(),
            },
        ],
    };
    let draft = DispatchDraft {
        owner: "alice".into(),
        repository: "repo".into(),
        branch: "main".into(),
        workflow: ".github/workflows/deploy.yml".into(),
        inputs: BTreeMap::from([(
            "input:.github/workflows/deploy.yml:retries".to_owned(),
            "4".to_owned(),
        )]),
        ..DispatchDraft::default()
    };
    let page = Page::DispatchRecovery {
        repo_chrome: Some(Box::new(repo_chrome())),
        base_path: "/alice/repo/actions".into(),
        csrf_token: "test-token".into(),
        message: "Run was not accepted. workflow input is not one of its options".into(),
        draft: Box::new(draft),
        workflows: vec![workflow],
    };
    let mut dom = VirtualDom::new_with_props(render, page);
    dom.rebuild_in_place();
    let rendered = dioxus_ssr::render(&dom);
    assert!(rendered.contains("not one of its options"), "{rendered}");
    assert!(rendered.contains("action=\"/alice/repo/actions/dispatch\""));
    assert!(rendered.contains("name=\"input:.github/workflows/deploy.yml:retries\""));
    assert!(rendered.contains("value=\"4\""), "{rendered}");
}

#[test]
fn global_run_form_takes_name_value_lines_and_keeps_them_on_refusal() {
    let draft = DispatchDraft {
        owner: "alice".into(),
        repository: "repo".into(),
        workflow: ".github/workflows/deploy.yml".into(),
        inputs_text: "target=dev\nretries=4".into(),
        ..DispatchDraft::default()
    };
    let page = Page::DispatchRecovery {
        repo_chrome: None,
        base_path: "/ci".into(),
        csrf_token: "test-token".into(),
        message: "Run was not accepted. workflow input is not one of its options".into(),
        draft: Box::new(draft),
        workflows: Vec::new(),
    };
    let mut dom = VirtualDom::new_with_props(render, page);
    dom.rebuild_in_place();
    let rendered = dioxus_ssr::render(&dom);
    assert!(rendered.contains("action=\"/ci/dispatch\""));
    assert!(rendered.contains("name=\"inputs\""), "{rendered}");
    assert!(rendered.contains("target=dev"), "{rendered}");
    assert!(rendered.contains("retries=4"), "{rendered}");
}

#[test]
fn event_filter_offers_the_supported_events_before_any_run() {
    let mut page = model();
    page.runs.clear();
    let rendered = html(page);
    assert!(rendered.contains("Any event"));
    for event in ["pull_request", "push", "workflow_dispatch"] {
        assert!(rendered.contains(&format!("value=\"{event}\"")), "{event}");
    }
}

#[test]
fn run_rows_and_empty_logs_present_clear_status_states() {
    let mut page = model();
    page.runs = ["queued", "running", "success", "failed"]
        .into_iter()
        .map(|status| {
            let mut run = run();
            run.id = format!("{status}-run");
            run.status = status.into();
            run
        })
        .collect();
    let rendered = html(page.clone());
    for (status, label) in [
        ("queued", "Queued"),
        ("running", "In progress"),
        ("success", "Success"),
        ("failed", "Failed"),
    ] {
        assert!(rendered.contains(&format!("ci-status-{status}")));
        assert!(rendered.contains(&format!(">{label}<")));
    }
    let queued = page.runs[0].clone();
    page.runs.clear();
    assert!(html(page.clone()).contains("No workflow runs yet"));
    page.selected = Some(SelectedRun {
        run: queued,
        logs: vec![],
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    });
    let rendered = html(page);
    assert!(rendered.contains("Waiting for the runner to start; logs appear here as the job runs."));
    assert!(rendered.contains("actions-log-meta"));
    assert!(rendered.contains("aria-label=\"Workflow logs\""));
}

#[test]
fn queued_run_header_states_its_status_once_and_what_it_waits_for() {
    let mut page = model();
    let mut queued = run();
    queued.status = "queued".into();
    queued.started_at = None;
    queued.completed_at = None;
    page.selected = Some(SelectedRun {
        run: queued,
        logs: vec![],
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    });
    let rendered = html(page);
    assert!(rendered.contains("Waiting for a runner"));
    assert!(!rendered.contains("· Queued"));
}

#[test]
fn act_step_markers_show_icons_and_keep_the_raw_line_as_title() {
    let mut page = model();
    let mut finished = run();
    finished.status = "success".into();
    let lines: Vec<_> = [
        "[CI/test] ⭐ Run Main checkout",
        "[CI/test] ✅ Success - Main checkout [15.6ms]",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, line)| LogLine {
        id: index as i64 + 1,
        stage: "ci".into(),
        line: line.into(),
        level: "info".into(),
        timestamp: 1,
    })
    .collect();
    page.selected = Some(SelectedRun {
        run: finished,
        fetched_lines: lines.len(),
        logs_truncated: false,
        logs: lines,
        deployment: None,
    });
    let rendered = html(page);
    assert!(rendered.contains("title=\"[CI/test] ✅ Success - Main checkout [15.6ms]\""));
    assert!(rendered.contains("actions-log-timing"));
    // The emoji survive only in the titles, not in the visible text.
    assert_eq!(rendered.matches("✅").count(), 1);
    assert_eq!(rendered.matches("⭐").count(), 1);
}

#[test]
fn selected_summary_groups_metadata_and_only_shows_recorded_deployment() {
    let mut page = model();
    let mut completed = run();
    completed.status = "success".into();
    completed.started_at = Some(10);
    completed.completed_at = Some(55);
    completed.head_sha = Some("a13f729abcdef".into());
    page.query.insert("view".into(), "summary".into());
    page.selected = Some(SelectedRun {
        run: completed,
        logs: vec![],
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    });
    let rendered = html(page.clone());
    assert_eq!(rendered.matches("<h1").count(), 1);
    assert!(rendered.contains("class=\"repo-content "));
    assert_actions_tab_active(&repo_tabs_html(), "/alice/repo");
    assert!(rendered.contains("actions-summary-grid"));
    assert!(rendered.contains("45s"));
    assert!(rendered.contains("/alice/repo/commit/a13f729abcdef"));
    assert!(rendered.contains("actions-metadata-grid"));
    assert!(rendered.contains("id=\"run-metadata\""));
    assert!(rendered.contains("Commit status"));
    assert!(rendered.contains("gitserv/ci"));
    assert!(!rendered.contains("id=\"job-logs\""));
    assert!(!rendered.contains("actions-deployment-card"));
    page.selected.as_mut().unwrap().deployment = Some(Deployment {
        environment: "staging".into(),
        status: "completed".into(),
        approved_at: Some(9),
        started_at: Some(10),
        completed_at: Some(55),
    });
    let rendered = html(page);
    assert!(rendered.contains("actions-deployment-card"));
    assert!(rendered.contains("staging"));
    assert!(rendered.contains("Approved"));
    assert!(rendered.contains("Completed"));
}

#[test]
fn restored_log_accordions_keep_all_markers_unknowns_failure_and_history_notice() {
    let mut page = model();
    let mut finished = run();
    finished.status = "failed".into();
    let lines: Vec<_> = [
        "Prelude before job markers",
        "[CI/test] ⭐ Run Main tests",
        "[CI/test] | failing assertion λ",
        "[CI/test] ❌ Failure - Main tests [2.1s]",
        "Unparsed trailer must remain visible in its accordion",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, line)| LogLine {
        id: index as i64 + 1,
        stage: "ci".into(),
        line: line.into(),
        level: if index == 2 { "error" } else { "info" }.into(),
        timestamp: 1,
    })
    .collect();
    page.log_limit = lines.len();
    // Logs show one job at a time; the lines after the prelude belong to the job named "test".
    page.query.insert("workflow_job".into(), "test".into());
    page.selected = Some(SelectedRun {
        run: finished,
        fetched_lines: lines.len(),
        logs_truncated: false,
        logs: lines,
        deployment: None,
    });
    let rendered = html(page);
    for id in 2..=5 {
        assert!(rendered.contains(&format!("id=\"log-{id}\"")));
    }
    assert!(rendered.contains("actions-step-list"));
    assert!(rendered.contains("actions-step-heading"));
    assert!(rendered.contains("actions-step-outcome"));
    assert!(rendered.contains("unknown"));
    assert!(rendered.contains("Failure - Main tests [2.1s]"));
    assert!(rendered.contains("Unparsed trailer must remain visible"));
    assert!(rendered.contains("actions-log-line is-error"));
    assert!(rendered.contains("Only the most recent 5 lines are shown"));
    assert!(rendered.contains("About these logs"));
    assert!(rendered.contains("name=\"log_q\""));
    assert!(rendered.contains("name=\"wrap\""));
}

#[test]
fn byte_limited_logs_explain_truncation_and_link_to_history() {
    let mut page = model();
    page.selected = Some(SelectedRun {
        run: run(),
        logs: vec![],
        deployment: None,
        fetched_lines: 1100,
        logs_truncated: true,
    });
    let rendered = html(page);
    assert!(rendered.contains("Recent logs were shortened to fit this page"));
    assert!(rendered.contains("Run status continues to update"));
    assert!(rendered.contains("href=\"/api/ci/jobs/real-run/history\""));
    assert!(!rendered.contains("Only the most recent 2000 lines"));
}

#[test]
fn workflow_source_links_use_the_run_commit_and_first_path() {
    let mut page = model();
    let mut run = run();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    run.head_sha = Some(sha.into());
    run.workflow_path = ".github/workflows/check λ.yml,.github/workflows/deploy.yml".into();
    assert_eq!(
        workflow_file_url(&page, &run).as_deref(),
        Some(
            "/alice/repo/blob/0123456789abcdef0123456789abcdef01234567/.github/workflows/check%20%CE%BB.yml"
        )
    );
    run.workflow_path = ".github/workflows/../private".into();
    assert!(workflow_file_url(&page, &run).is_none());
    run.workflow_path = "default".into();
    assert!(workflow_file_url(&page, &run).is_none());
    run.workflow_path = ".github/workflows/ci.yml".into();
    run.head_sha = None;
    assert!(workflow_file_url(&page, &run).is_none());
    page.base_path = "/ci".into();
    assert!(repository_base(&page.base_path).is_none());
    assert!(repository_base("/alice/repo/extra/actions").is_none());
    assert_eq!(repository_base("/alice/repo/actions"), Some("/alice/repo"));
}

#[test]
fn safe_page_model_round_trips_for_shared_hydration() {
    let page = Page::Actions(Box::new(model()));
    let json = serde_json::to_string(&page).unwrap();
    let decoded: Page = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, page);
    let mut legacy = serde_json::to_value(&page).unwrap();
    legacy["Actions"]
        .as_object_mut()
        .unwrap()
        .remove("repo_chrome");
    let decoded: Page = serde_json::from_value(legacy).unwrap();
    let Page::Actions(model) = decoded else {
        panic!("expected Actions page")
    };
    assert!(model.repo_chrome.is_none());
    assert_eq!(model.repository, Some(("alice".into(), "repo".into())));
    assert!(!json.contains("disk_path"));
    assert!(!json.contains("password_hash"));
}

#[test]
fn log_filter_key_resets_local_controls_without_changing_the_run() {
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Harness(Rc<RefCell<Option<Signal<ActionsPage>>>>);
    fn root(props: Harness) -> Element {
        let mut initial = model();
        initial.query.insert("log_q".into(), "first".into());
        initial.selected = Some(SelectedRun {
            run: run(),
            logs: vec![],
            deployment: None,
            fetched_lines: 0,
            logs_truncated: false,
        });
        let page = use_signal(|| initial);
        *props.0.borrow_mut() = Some(page);
        actions_page(page())
    }
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(root, Harness(control.clone()));
    dom.rebuild_in_place();
    let mut page = control.borrow().unwrap();
    assert!(dioxus_ssr::render(&dom).contains("value=\"first\""));
    dom.in_runtime(|| {
        page.write().query.insert("log_q".into(), "second".into());
    });
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("value=\"second\""),
        "changed log identity kept an old local search: {html}"
    );
    assert!(!html.contains("value=\"first\""));
    assert!(html.contains("real-run"));
}

#[test]
fn a_repository_without_workflows_gets_started_instead_of_the_run_list() {
    let mut page = model();
    page.workflows.clear();
    page.runs.clear();
    let rendered = html(page.clone());
    assert!(rendered.contains("Get started with Actions"));
    assert!(rendered.contains("<code>.github/workflows/</code>"));
    assert!(rendered.contains("runs-on: ubuntu-latest"));
    assert!(rendered.contains("href=\"/alice/repo/new/main/.github/workflows\""));
    assert!(!rendered.contains("actions-run-toolbar"));
    assert!(!rendered.contains("Filter workflow runs"));
    assert!(!rendered.contains("Run on"));
    assert!(!rendered.contains("View workflows"));
    assert!(!rendered.contains(">Refresh<"));
    assert!(!rendered.contains("Showing the latest"));
    assert!(!rendered.contains("Show more runs"));
    page.repo_chrome.as_mut().unwrap().can_write = false;
    let reader = html(page);
    assert!(reader.contains("Get started with Actions"));
    assert!(!reader.contains("Create a workflow file"));
}

#[test]
fn a_repository_with_workflows_but_no_runs_keeps_the_toolbar_without_the_run_footer() {
    let mut page = model();
    page.runs.clear();
    let rendered = html(page);
    assert!(rendered.contains("actions-run-toolbar"));
    assert!(rendered.contains("Filter by branch"));
    assert!(rendered.contains("placeholder=\"Branch\""));
    assert!(!rendered.contains("Get started with Actions"));
    assert!(!rendered.contains("Showing the latest"));
    assert!(!rendered.contains("Show more runs"));
}

#[test]
fn run_workflow_sits_beside_refresh_and_the_filter_bar_holds_only_filters() {
    let rendered = html(model());
    assert_eq!(rendered.matches("id=\"actions-run-filters\"").count(), 1);
    let filters = rendered
        .split_once("id=\"actions-run-filters\"")
        .unwrap()
        .1
        .split_once("</form>")
        .unwrap()
        .0;
    assert!(filters.contains("Filter runs"));
    assert!(!filters.contains("Run workflow"), "{filters}");
    let controls = rendered.find("class=\"actions-heading-controls\"").unwrap();
    let run = rendered.find("Run workflow").unwrap();
    let refresh = rendered.find(">Refresh<").unwrap();
    let toolbar = rendered.find("class=\"actions-run-toolbar\"").unwrap();
    assert!(
        controls < run && run < refresh && refresh < toolbar,
        "{rendered}"
    );
}

#[test]
fn the_runs_footer_shows_only_when_the_page_is_full() {
    let partial = html(model());
    assert!(!partial.contains("Showing the latest"));
    assert!(!partial.contains("Show more runs"));
    let mut full = model();
    full.run_limit = 1;
    let rendered = html(full);
    assert!(rendered.contains("Showing the latest 1 runs."));
    assert!(rendered.contains("Show more runs"));
}

#[test]
fn run_page_offers_cancel_and_rerun_forms_only_to_writers() {
    let selected = |run: Run| SelectedRun {
        run,
        logs: vec![],
        deployment: None,
        fetched_lines: 0,
        logs_truncated: false,
    };
    let mut page = model();
    let mut running = run();
    running.head_sha = Some("a".repeat(40));
    page.selected = Some(selected(running.clone()));
    let rendered = html(page.clone());
    assert!(
        rendered.contains("action=\"/alice/repo/actions/runs/real-run/cancel\""),
        "{rendered}"
    );
    assert!(!rendered.contains("/rerun\""), "{rendered}");
    assert!(rendered.contains("name=\"csrf_token\""), "{rendered}");

    // Readers get neither form.
    page.can_dispatch = false;
    let reader = html(page.clone());
    assert!(!reader.contains("/cancel\""), "{reader}");
    assert!(!reader.contains("/rerun\""), "{reader}");
    page.can_dispatch = true;

    // A finished run is run again, whatever its event.
    running.status = "succeeded".into();
    page.selected = Some(selected(running.clone()));
    let finished = html(page.clone());
    assert!(
        finished.contains("action=\"/alice/repo/actions/runs/real-run/rerun\""),
        "{finished}"
    );
    assert!(!finished.contains("/cancel\""), "{finished}");
    running.event = "pull_request".into();
    page.selected = Some(selected(running));
    assert!(html(page).contains("action=\"/alice/repo/actions/runs/real-run/rerun\""));
}
