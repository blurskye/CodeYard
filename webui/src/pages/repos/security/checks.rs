//! Read-only projections of durable SARIF analyses on a pull request's current revision.
use crate::navigation::NavLink;
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Annotation {
    pub number: i64,
    pub rule: String,
    pub message: String,
    pub path: String,
    pub line: i64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Check {
    pub name: String,
    pub state: String,
    pub description: String,
    pub url: String,
    pub annotations: Vec<Annotation>,
    pub truncated: bool,
}

pub fn check_list(checks: &[Check]) -> Element {
    rsx! {
        if !checks.is_empty() {
            section { class: "collaboration-panel", aria_label: "Code scanning checks",
                h3 { "Code scanning" }
                for check in checks {
                    article { class:"repo-security-check",
                        h4 { NavLink { href:check.url.clone(), "{check.name}" } " · {check.state}" }
                        p { "{check.description}" }
                        for annotation in &check.annotations { {annotation_block(annotation)} }
                        if check.truncated { p { "Some annotations are omitted to keep this page bounded. The Security page lists every finding." } }
                    }
                }
            }
        }
    }
}

pub fn annotation_block(annotation: &Annotation) -> Element {
    rsx! {
        aside { class:"collaboration-diff-thread repo-security-annotation", aria_label:"Code scanning annotation",
            strong { NavLink { href:annotation.url.clone(), "{annotation.rule} #{annotation.number}" } }
            p { "{annotation.message}" }
            small { "{annotation.path}:{annotation.line}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_scanning_annotations_escape_scanner_text_and_explain_truncation() {
        let checks = vec![Check {
            name: "Semgrep / <script>".into(),
            state: "failure".into(),
            description: "1 new finding".into(),
            url: "/alice/repo/security".into(),
            truncated: true,
            annotations: vec![Annotation {
                number: 1,
                rule: "<script>rule</script>".into(),
                message: "<img src=x onerror=alert(1)>".into(),
                path: "src/<script>.rs".into(),
                line: 3,
                url: "/alice/repo/security/code-scanning/1".into(),
            }],
        }];
        let html = crate::tests::in_scope(|| dioxus_ssr::render_element(check_list(&checks)));
        assert!(html.contains("aria-label=\"Code scanning annotation\""));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&#60;script&#62;") || html.contains("&lt;script&gt;"));
        assert!(html.contains("Some annotations are omitted"));
        let encoded = serde_json::to_vec(&checks).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<Check>>(&encoded).unwrap(),
            checks
        );
    }
}
