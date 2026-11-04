use lettre::message::Mailbox;
use std::str::FromStr;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Starttls,
    Tls,
    /// No encryption, for a relay on a trusted network.
    Plain,
}

impl Security {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::Starttls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::Plain),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starttls => "starttls",
            Self::Tls => "tls",
            Self::Plain => "none",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Starttls => 587,
            Self::Tls => 465,
            Self::Plain => 25,
        }
    }
}

/// Where the settings in use come from. The database wins once an administrator has saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Database,
    Environment,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub password: Option<String>,
    pub from_name: String,
    pub from_address: String,
    pub reply_to: String,
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[set]"))
            .field("from_address", &self.from_address)
            .finish()
    }
}

/// A blank port means the usual port for the security mode.
pub fn parse_port(text: &str, security: Security) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(security.default_port());
    }
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err("The port must be a number from 1 to 65535.".to_owned()),
    }
}

impl SmtpConfig {
    /// Checks the fields the same way for the admin form and for the environment.
    pub fn validate(&self) -> Result<(), String> {
        if self.host.is_empty()
            || self.host.len() > 255
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("Enter the mail server host name.".to_owned());
        }
        if self.username.len() > 255 || self.username.chars().any(char::is_control) {
            return Err("The username is not valid.".to_owned());
        }
        if self.from_address.len() > 255 || lettre::Address::from_str(&self.from_address).is_err() {
            return Err("Enter a valid From address.".to_owned());
        }
        if self.from_name.len() > 255 || self.from_name.chars().any(char::is_control) {
            return Err("The From name is not valid.".to_owned());
        }
        if !self.reply_to.is_empty()
            && (self.reply_to.len() > 255 || lettre::Address::from_str(&self.reply_to).is_err())
        {
            return Err("Enter a valid Reply-To address.".to_owned());
        }
        Ok(())
    }
}

/// Reads the CODEYARD_SMTP_* variables. `None` when no host is set. `FROM` may be a bare
/// address or `Name <address>`.
pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Option<SmtpConfig>, String> {
    let get = |name: &str| {
        lookup(&format!("CODEYARD_SMTP_{name}"))
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let host = get("HOST");
    if host.is_empty() {
        return Ok(None);
    }
    let security = match get("SECURITY").as_str() {
        "" => Security::Starttls,
        value => Security::parse(value)
            .ok_or("CODEYARD_SMTP_SECURITY must be starttls, tls or none.".to_owned())?,
    };
    let from = Mailbox::from_str(&get("FROM"))
        .map_err(|_| "CODEYARD_SMTP_FROM must be an address or Name <address>.".to_owned())?;
    // The password is read as it is: spaces at its ends belong to it, and an empty value is none.
    let password = lookup("CODEYARD_SMTP_PASSWORD").filter(|value| !value.is_empty());
    let config = SmtpConfig {
        host,
        port: parse_port(&get("PORT"), security)
            .map_err(|_| "CODEYARD_SMTP_PORT must be a number from 1 to 65535.".to_owned())?,
        security,
        username: get("USERNAME"),
        password,
        from_name: from.name.unwrap_or_default(),
        from_address: from.email.to_string(),
        reply_to: get("REPLY_TO"),
    };
    config.validate()?;
    Ok(Some(config))
}

pub fn from_env() -> Result<Option<SmtpConfig>, String> {
    from_lookup(&|name| std::env::var(name).ok())
}

/// Whether the environment sets up a mail server. Saved settings take precedence over it.
pub fn environment_configured() -> bool {
    matches!(from_env(), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn no_host_means_no_mail_server() {
        assert_eq!(from_lookup(&env(&[])), Ok(None));
        assert_eq!(from_lookup(&env(&[("CODEYARD_SMTP_HOST", "  ")])), Ok(None));
    }

    #[test]
    fn environment_gives_defaults_for_port_and_security() {
        let config = from_lookup(&env(&[
            ("CODEYARD_SMTP_HOST", "mail.example.com"),
            ("CODEYARD_SMTP_FROM", "Codeyard <noreply@example.com>"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.security, Security::Starttls);
        assert_eq!(config.port, 587);
        assert_eq!(config.from_name, "Codeyard");
        assert_eq!(config.from_address, "noreply@example.com");
        assert_eq!(config.password, None);
    }

    #[test]
    fn environment_values_are_checked() {
        let host = ("CODEYARD_SMTP_HOST", "mail.example.com");
        let from = ("CODEYARD_SMTP_FROM", "noreply@example.com");
        assert!(from_lookup(&env(&[host])).is_err());
        assert!(from_lookup(&env(&[host, from, ("CODEYARD_SMTP_PORT", "0")])).is_err());
        assert!(from_lookup(&env(&[host, from, ("CODEYARD_SMTP_SECURITY", "ssl")])).is_err());
        let tls = from_lookup(&env(&[host, from, ("CODEYARD_SMTP_SECURITY", "TLS")]))
            .unwrap()
            .unwrap();
        assert_eq!((tls.security, tls.port), (Security::Tls, 465));
    }

    #[test]
    fn the_debug_output_hides_the_password() {
        let config = from_lookup(&env(&[
            ("CODEYARD_SMTP_HOST", "mail.example.com"),
            ("CODEYARD_SMTP_FROM", "noreply@example.com"),
            ("CODEYARD_SMTP_PASSWORD", "hunter2"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.password.as_deref(), Some("hunter2"));
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn the_password_keeps_its_surrounding_spaces() {
        let config = from_lookup(&env(&[
            ("CODEYARD_SMTP_HOST", "mail.example.com"),
            ("CODEYARD_SMTP_FROM", "noreply@example.com"),
            ("CODEYARD_SMTP_PASSWORD", " secret "),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.password.as_deref(), Some(" secret "));
        let empty = from_lookup(&env(&[
            ("CODEYARD_SMTP_HOST", "mail.example.com"),
            ("CODEYARD_SMTP_FROM", "noreply@example.com"),
            ("CODEYARD_SMTP_PASSWORD", ""),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(empty.password, None);
    }
}
