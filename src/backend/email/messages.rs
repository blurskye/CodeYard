//! The texts of the mail the site sends. Plain text and short. `origin` is the scheme and host
//! from PUBLIC_URL; without it a mail has no links, since a bare path does not work from a mail
//! client.

pub struct Mail {
    pub subject: String,
    pub body: String,
}

/// A link to a page of this site, or `None` without an origin.
fn site_link(origin: &str, path: &str) -> Option<String> {
    if path.starts_with("http://") || path.starts_with("https://") {
        Some(path.to_owned())
    } else {
        (!origin.is_empty()).then(|| format!("{origin}{path}"))
    }
}

/// A header holds one line. Control characters become spaces and long titles are cut.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub fn notification(origin: &str, title: &str, body: Option<&str>, link: Option<&str>) -> Mail {
    let mut text = String::new();
    if let Some(body) = body.map(str::trim).filter(|body| !body.is_empty()) {
        text.push_str(body);
        text.push_str("\n\n");
    }
    if let Some(link) = link
        .filter(|link| !link.is_empty())
        .and_then(|link| site_link(origin, link))
    {
        text.push_str(&link);
        text.push_str("\n\n");
    }
    text.push_str("--\nYou get this because of your notification settings");
    match site_link(origin, "/settings?tab=email") {
        Some(settings) => text.push_str(&format!(": {settings}\n")),
        None => text.push_str(".\n"),
    }
    Mail {
        subject: one_line(title),
        body: text,
    }
}

pub fn signup_decision(origin: &str, username: &str, approved: bool) -> Mail {
    let (subject, line) = if approved {
        let line = match site_link(origin, "/login") {
            Some(login) => format!("You can now sign in as {username}: {login}"),
            None => format!("You can now sign in as {username}."),
        };
        ("Your Codeyard account request was approved", line)
    } else {
        (
            "Your Codeyard account request was declined",
            "An administrator declined your request for an account.".to_owned(),
        )
    };
    Mail {
        subject: subject.to_owned(),
        body: format!("Hello {username},\n\n{line}\n"),
    }
}

/// The link line of a code mail, when there is an origin.
fn link_line(origin: &str, text: &str, path: &str) -> String {
    site_link(origin, path)
        .map(|link| format!("{text} {link}\n\n"))
        .unwrap_or_default()
}

pub fn verify_email(origin: &str, username: &str, token: &str) -> Mail {
    Mail {
        subject: "Confirm your Codeyard email address".to_owned(),
        body: format!(
            "Your code:\n{token}\n\nHello {username}, paste this code under Settings, Email to confirm this address for your Codeyard account.\n\n{}The code works for 24 hours. If you did not ask for this, ignore this email.\n",
            link_line(origin, "Or open this link:", &format!("/verify-email?token={token}"))
        ),
    }
}

pub fn password_reset(origin: &str, username: &str, token: &str) -> Mail {
    Mail {
        subject: "Reset your Codeyard password".to_owned(),
        body: format!(
            "Your code:\n{token}\n\nHello {username}, paste this code with your email address on the password reset page to choose a new password for your Codeyard account.\n\n{}The code works for one hour and only once. If you did not ask for this, ignore this email; your password stays the same.\n",
            link_line(origin, "Or open this link:", &format!("/reset-password?token={token}"))
        ),
    }
}

pub fn test_message() -> Mail {
    Mail {
        subject: "Codeyard test email".to_owned(),
        body: "This is a test email from your Codeyard server. Email is set up correctly.\n"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "https://git.example.com";

    #[test]
    fn a_notification_has_its_text_and_a_link() {
        let mail = notification(
            SITE,
            "alice mentioned you",
            Some("  hi @bob  "),
            Some("/o/r/issues/3"),
        );
        assert_eq!(mail.subject, "alice mentioned you");
        assert!(mail.body.starts_with("hi @bob\n\n"));
        assert!(mail.body.contains("https://git.example.com/o/r/issues/3\n"));
        assert!(mail
            .body
            .contains("https://git.example.com/settings?tab=email"));
    }

    #[test]
    fn without_an_origin_a_notification_has_no_links() {
        let mail = notification("", "alice mentioned you", Some("hi"), Some("/o/r/issues/3"));
        assert!(!mail.body.contains("/o/r/issues/3"), "{}", mail.body);
        assert!(!mail.body.contains("/settings"), "{}", mail.body);
        assert!(mail.body.contains("notification settings.\n"));
    }

    #[test]
    fn a_title_cannot_add_headers() {
        let mail = notification(SITE, "hello\r\nBcc: someone@example.com", None, None);
        assert_eq!(mail.subject, "hello  Bcc: someone@example.com");
        assert!(!mail.subject.contains('\n'));
        let long = notification(SITE, &"x".repeat(500), None, None);
        assert_eq!(long.subject.chars().count(), 200);
    }

    #[test]
    fn code_mail_starts_with_the_code_and_links_only_with_an_origin() {
        let verify = verify_email(SITE, "carol", "abc123");
        assert!(verify.body.starts_with("Your code:\nabc123\n\n"));
        assert!(verify
            .body
            .contains("https://git.example.com/verify-email?token=abc123\n"));
        let reset = password_reset(SITE, "carol", "def456");
        assert!(reset.body.starts_with("Your code:\ndef456\n\n"));
        assert!(reset
            .body
            .contains("https://git.example.com/reset-password?token=def456\n"));
        assert!(reset.body.contains("Hello carol,"));
        for mail in [
            verify_email("", "carol", "abc123"),
            password_reset("", "carol", "def456"),
        ] {
            assert!(mail.body.starts_with("Your code:\n"));
            assert!(!mail.body.contains("token="), "{}", mail.body);
            assert!(!mail.body.contains("http"), "{}", mail.body);
        }
    }

    #[test]
    fn signup_mail_names_the_decision() {
        assert!(signup_decision(SITE, "carol", true)
            .subject
            .contains("approved"));
        assert!(signup_decision(SITE, "carol", true).body.contains("/login"));
        assert!(!signup_decision("", "carol", true).body.contains("/login"));
        assert!(signup_decision(SITE, "carol", false)
            .subject
            .contains("declined"));
        assert!(!signup_decision(SITE, "carol", false)
            .body
            .contains("/login"));
    }
}
