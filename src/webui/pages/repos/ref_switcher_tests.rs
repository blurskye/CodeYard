use super::*;

#[test]
fn branch_menu_qualifies_names_even_when_the_name_starts_with_refs() {
    let options = ref_options("/u/r", "tree", "docs", &["refs/heads/topic".into()], "");
    assert_eq!(
        options[0].href,
        "/u/r/tree/refs/heads/refs/heads/topic/docs?root_if_missing=1"
    );
}

#[test]
fn option_urls_and_rendered_matches_are_bounded_and_borrowed() {
    let names = (0..10_000)
        .map(|index| format!("branch-{index}"))
        .collect::<Vec<_>>();
    let options = ref_options("/u/r", "blob", &"a".repeat(3072), &names, "");
    assert_eq!(options.len(), MAX_REF_OPTIONS);
    assert!(
        options
            .iter()
            .map(|option| option.href.len())
            .sum::<usize>()
            < 200_000
    );
    let matches = matching(&options, "branch-1");
    assert!(std::ptr::eq(matches[0], &options[1]));
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(rsx! {
            RefSwitcher { base: "/u/r", current: "main", branches: options.clone(), tags: vec![] }
        })
    });
    assert!(html.contains("View all branches"));
    assert!(html.contains("View all tags"));
    assert!(html.len() < 250_000);
}
