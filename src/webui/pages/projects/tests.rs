use super::*;

fn page() -> Page {
    let item = Item {
        id: 1,
        title: "<script>Draft</script>".into(),
        url: None,
        body: "Notes".into(),
        values: vec![(2, "todo".into(), "Todo".into())],
    };
    Page {
        view_state: Default::default(),
        owner: "alice".into(),
        base: "/users/alice/projects".into(),
        csrf_token: "csrf-marker".into(),
        can_edit: true,
        projects: vec![],
        project: Some(Project {
            number: 1,
            title: "Roadmap".into(),
            description: "Plans".into(),
            public: true,
            closed: false,
        }),
        fields: vec![Field {
            id: 2,
            name: "Status".into(),
            data_type: "single_select".into(),
            options: vec![("todo".into(), "Todo".into())],
        }],
        settings: false,
        items: vec![item.clone()],
        columns: vec![Column {
            name: "Todo".into(),
            items: vec![item],
        }],
        views: vec![
            View {
                number: 1,
                name: "Table".into(),
            },
            View {
                number: 2,
                name: "Board".into(),
            },
        ],
        view_number: 2,
        filter: String::new(),
        message: None,
        previous: None,
        next: None,
    }
}

#[test]
fn project_board_uses_plain_forms_and_escapes_content() {
    let html = crate::webui::tests::in_scope(|| dioxus_ssr::render_element(render(page())));
    assert!(
        html.contains("method=\"post\"") && html.contains("action=\"/users/alice/projects/1\""),
        "{html}"
    );
    assert!(
        html.contains("name=\"value_2\"") && html.contains("value=\"todo\""),
        "{html}"
    );
    assert!(
        html.contains("&#60;script&#62;Draft&#60;/script&#62;") && !html.contains("<script>Draft"),
        "{html}"
    );
    assert!(
        html.contains("min(100%,260px)"),
        "the board columns must fit narrow screens: {html}"
    );
    assert!(html.contains("csrf-marker"));
}

#[test]
fn project_table_and_settings_survive_hydration() {
    let mut model = page();
    model.columns.clear();
    let encoded = serde_json::to_string(&model).unwrap();
    assert_eq!(serde_json::from_str::<Page>(&encoded).unwrap(), model);
    let html = crate::webui::tests::in_scope(|| dioxus_ssr::render_element(render(model.clone())));
    assert!(html.contains("<table") && html.contains("overflow-x:auto"));
    model.settings = true;
    let html = crate::webui::tests::in_scope(|| dioxus_ssr::render_element(render(model)));
    assert!(html.contains("Delete project") && html.contains("Iteration duration in days"));
}
