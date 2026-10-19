//! What a wiki page may say and be called. The database stores pages; these rules decide the
//! title, the address derived from it and the size limits.

use crate::webui::pages::repos::wiki::slug_for;

pub const TITLE_LIMIT: usize = 200;
pub const BODY_LIMIT: usize = 1024 * 1024;
pub const MESSAGE_LIMIT: usize = 500;

/// Addresses the wiki uses for its own pages, so no page takes them.
const RESERVED: [&str; 3] = ["_pages", "_new", "_settings"];

/// A page the writer typed, checked and ready to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub slug: String,
    pub title: String,
    pub body: String,
    pub message: String,
}

/// Checks what a writer submitted. The error is the text the page shows.
pub fn check(title: &str, body: &str, message: &str) -> Result<Draft, &'static str> {
    let title = title.trim();
    // PostgreSQL text cannot hold NUL, so a page holding one could not be copied to it.
    if title.contains('\0') || body.contains('\0') || message.contains('\0') {
        return Err("Text cannot contain NUL characters.");
    }
    if title.is_empty() {
        return Err("Enter a title for the page.");
    }
    if title.chars().count() > TITLE_LIMIT {
        return Err("The title is longer than 200 characters.");
    }
    if body.len() > BODY_LIMIT {
        return Err("The page is larger than 1 MiB.");
    }
    if message.chars().count() > MESSAGE_LIMIT {
        return Err("The edit message is longer than 500 characters.");
    }
    let slug = slug_for(title).ok_or("The title needs a letter or a number.")?;
    if slug.chars().count() > TITLE_LIMIT {
        return Err("The title is longer than 200 characters.");
    }
    if RESERVED.contains(&slug.as_str()) {
        return Err("This title is reserved for the wiki's own pages.");
    }
    Ok(Draft {
        slug,
        title: title.to_owned(),
        body: body.to_owned(),
        message: message.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_and_hyphenated() {
        assert_eq!(
            slug_for("Getting Started!").as_deref(),
            Some("getting-started")
        );
        assert_eq!(slug_for("  Home  ").as_deref(), Some("home"));
        assert_eq!(slug_for("_Sidebar").as_deref(), Some("_sidebar"));
        assert_eq!(slug_for("a -- b").as_deref(), Some("a-b"));
        assert_eq!(slug_for("!!!"), None);
    }

    #[test]
    fn check_refuses_reserved_addresses_and_oversized_text() {
        assert_eq!(
            check("_Pages", "", "").unwrap_err(),
            "This title is reserved for the wiki's own pages."
        );
        assert_eq!(
            check("_Settings", "", "").unwrap_err(),
            "This title is reserved for the wiki's own pages."
        );
        assert_eq!(
            check("  ", "", "").unwrap_err(),
            "Enter a title for the page."
        );
        let long = "a".repeat(TITLE_LIMIT + 1);
        assert!(check(&long, "", "").is_err());
        let big = "a".repeat(BODY_LIMIT + 1);
        assert!(check("Home", &big, "").is_err());
        let draft = check(" Home ", "text", " fix ").unwrap();
        assert_eq!(draft.slug, "home");
        assert_eq!(draft.title, "Home");
        assert_eq!(draft.message, "fix");
    }

    #[test]
    fn check_refuses_nul_in_title_body_and_message() {
        // PostgreSQL text cannot hold NUL, so a SQLite wiki holding one could not be copied.
        assert!(check("Home\0", "", "").is_err());
        assert!(check("Home", "text\0", "").is_err());
        assert!(check("Home", "text", "fix\0").is_err());
        assert!(check("Home", "text", "fix").is_ok());
    }
}
