//! `[[Page Name]]` and `[[text|Page Name]]` in wiki Markdown. They become ordinary links to the
//! page, or to the new-page form when no page has that address yet.
use super::slug_for;

/// Rewrites the wiki links of `body` into Markdown links. `existing` lists the addresses of the
/// pages that exist. A link whose target has no address is left as written.
pub fn expand(body: &str, base: &str, existing: &[String]) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start + 2..].find("]]") else {
            break;
        };
        let inner = &rest[start + 2..start + 2 + len];
        let after = start + 2 + len + 2;
        out.push_str(&rest[..start]);
        rest = &rest[after..];
        let (text, target) = match inner.split_once('|') {
            Some((text, target)) => (text.trim(), target.trim()),
            None => (inner.trim(), inner.trim()),
        };
        match slug_for(target) {
            Some(slug) if !text.is_empty() => {
                let url = if existing.contains(&slug) {
                    format!("{base}/wiki/{}", crate::webui::encode_component(&slug))
                } else {
                    format!(
                        "{base}/wiki/_new?title={}",
                        crate::webui::encode_component(target)
                    )
                };
                let text = text.replace('[', "\\[").replace(']', "\\]");
                out.push_str(&format!("[{text}]({url})"));
            }
            _ => out.push_str(&format!("[[{inner}]]")),
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn wiki_links_point_at_existing_pages_or_the_new_page_form() {
        let existing = vec!["install-guide".to_owned()];
        let text = expand(
            "See [[Install Guide]] and [[the guide|Install Guide]] and [[Missing Page]].",
            "/a/b",
            &existing,
        );
        assert_eq!(
            text,
            "See [Install Guide](/a/b/wiki/install-guide) and [the guide](/a/b/wiki/install-guide) and [Missing Page](/a/b/wiki/_new?title=Missing%20Page)."
        );
    }

    #[test]
    fn unclosed_or_empty_links_stay_as_written() {
        assert_eq!(expand("[[open", "/a/b", &[]), "[[open");
        assert_eq!(expand("[[ ]]", "/a/b", &[]), "[[ ]]");
    }
}
