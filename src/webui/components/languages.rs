//! A repository's language mix: a bar of coloured segments with a legend. The repository page
//! shows the bar in its About panel. Repository cards show the primary language's colour dot.
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// One language of a repository: its name, its share of the code and the colour it is drawn in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LanguageShare {
    pub name: String,
    /// The share of the repository's code, from 0 to 100.
    pub percentage: f64,
    pub color: String,
}

/// The bar and its legend, most used first. Each segment's tooltip gives the language and its
/// share. Renders nothing for a repository with no languages.
pub fn language_bar(languages: &[LanguageShare]) -> Element {
    let summary = languages
        .iter()
        .map(|language| format!("{} {}", language.name, percent_label(language.percentage)))
        .collect::<Vec<_>>()
        .join(", ");
    let segments: Vec<(&LanguageShare, String, String)> = languages
        .iter()
        .map(|language| {
            (
                language,
                percent_label(language.percentage),
                format!("width: {:.2}%", language.percentage),
            )
        })
        .collect();
    rsx! {
        if !languages.is_empty() {
            div { class: "language-block",
                h3 { class: "language-heading", "Languages" }
                div { class: "language-bar", role: "img", aria_label: "Languages: {summary}",
                    for (language, percent, width) in &segments {
                        span { class: "language-bar-segment", style: "{width}; background: {language.color}", title: "{language.name} {percent}" }
                    }
                }
                ul { class: "language-legend",
                    for (language, percent, _) in &segments {
                        li {
                            span { class: "language-dot", style: "background: {language.color}", aria_hidden: "true" }
                            span { class: "language-name", "{language.name}" }
                            span { class: "language-percent", "{percent}" }
                        }
                    }
                }
            }
        }
    }
}

/// A share as the legend and the tooltips print it: one decimal place and a percent sign.
fn percent_label(percentage: f64) -> String {
    format!("{percentage:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(name: &str, percentage: f64, color: &str) -> LanguageShare {
        LanguageShare {
            name: name.into(),
            percentage,
            color: color.into(),
        }
    }

    #[test]
    fn bar_draws_each_language_with_its_colour_share_and_tooltip() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(language_bar(&[
                share("Rust", 92.5, "#dea584"),
                share("Shell", 7.5, "#89e051"),
            ]))
        });
        assert!(html.contains("role=\"img\""), "{html}");
        assert!(
            html.contains("aria-label=\"Languages: Rust 92.5%, Shell 7.5%\""),
            "{html}"
        );
        assert!(
            html.contains("width: 92.50%; background: #dea584"),
            "segment width and colour: {html}"
        );
        assert!(
            html.contains("title=\"Rust 92.5%\""),
            "tooltip with name and share: {html}"
        );
        assert!(
            html.contains("title=\"Shell 7.5%\""),
            "tooltip with name and share: {html}"
        );
        assert!(
            html.contains("<h3 class=\"language-heading\">Languages</h3>"),
            "{html}"
        );
        assert!(html.contains("class=\"language-name\">Rust<"), "{html}");
        assert!(html.contains("class=\"language-percent\">7.5%<"), "{html}");
        assert!(
            html.contains("class=\"language-dot\" style=\"background: #89e051\""),
            "legend colour dot: {html}"
        );
    }

    #[test]
    fn bar_escapes_language_names_and_renders_nothing_without_languages() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(language_bar(&[share("<script>", 100.0, "#000000")]))
        });
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&#60;script&#62;"), "{html}");

        let empty = crate::webui::tests::in_scope(|| dioxus_ssr::render_element(language_bar(&[])));
        assert!(!empty.contains("language-bar"), "{empty}");
        assert!(!empty.contains("language-legend"), "{empty}");
    }
}
