use super::*;

fn files(path: &str) -> RefLinks {
    RefLinks::Files {
        base: "/u/r".into(),
        kind: "tree",
        path: path.into(),
    }
}

#[test]
fn branch_menu_qualifies_names_even_when_the_name_starts_with_refs() {
    assert_eq!(
        files("docs").href("refs/heads/topic", false),
        "/u/r/tree/refs/heads/refs/heads/topic/docs?root_if_missing=1"
    );
    assert_eq!(files("").href("v1", true), "/u/r/tree/refs/tags/v1");
}

#[test]
fn the_filter_reaches_names_past_the_drawn_limit() {
    let names = (0..200)
        .map(|index| format!("branch-{index}"))
        .collect::<Vec<_>>();
    let matches = matching(&names, "branch-1");
    assert_eq!(matches.len(), MAX_REF_OPTIONS);
    assert!(std::ptr::eq(matches[0], &names[1]));
    assert_eq!(matching(&names, "branch-199").len(), 1);
    let links = files(&"a".repeat(3072));
    let html = crate::tests::in_scope(|| {
        dioxus_ssr::render_element(rsx! {
            RefSwitcher { base: "/u/r", current: "main", branches: names.clone(), tags: vec![], links: links.clone() }
        })
    });
    assert!(html.contains("View all branches"));
    assert!(html.contains("View all tags"));
    assert!(html.len() < 250_000);
}
