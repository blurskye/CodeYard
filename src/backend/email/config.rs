//! The mail server settings: read from the database row or from CODEYARD_SMTP_* variables.
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::{self, models::EmailSettings, DbPool};
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
    let password = get("PASSWORD");
    let config = SmtpConfig {
        host,
        port: parse_port(&get("PORT"), security)
            .map_err(|_| "CODEYARD_SMTP_PORT must be a number from 1 to 65535.".to_owned())?,
        security,
        username: get("USERNAME"),
        password: (!password.is_empty()).then_some(password),
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

fn unavailable(error: impl std::fmt::Display) -> String {
    tracing::warn!(%error, "could not read email settings");
    "The email settings are unavailable.".to_owned()
}

fn from_row(row: EmailSettings, password: Option<String>) -> Result<SmtpConfig, String> {
    Ok(SmtpConfig {
        security: Security::parse(&row.security).ok_or_else(|| unavailable("bad security"))?,
        port: u16::try_from(row.port).map_err(unavailable)?,
        host: row.host,
        username: row.username,
        password,
        from_name: row.from_name,
        from_address: row.from_address,
        reply_to: row.reply_to,
    })
}

/// The settings in use: the saved ones, else the environment's. The error text is safe to show
/// an administrator.
pub async fn load(
    pool: &DbPool,
    secrets: Option<&SecretEncryptor>,
) -> Result<Option<(SmtpConfig, Source)>, String> {
    let Some(row) = db::email::get_email_settings(pool)
        .await
        .map_err(unavailable)?
    else {
        return Ok(from_env()?.map(|config| (config, Source::Environment)));
    };
    let password = match (&row.encrypted_password, &row.password_nonce) {
        (Some(ciphertext), Some(nonce)) => {
            let secrets =
                secrets.ok_or("SECRETS_MASTER_KEY is needed to use the saved mail password.")?;
            let bytes = secrets
                .decrypt(ciphertext, nonce)
                .map_err(|_| "The saved mail password cannot be opened with SECRETS_MASTER_KEY.")?;
            Some(String::from_utf8(bytes).map_err(unavailable)?)
        }
        _ => None,
    };
    Ok(Some((from_row(row, password)?, Source::Database)))
}

/// The settings in use without opening the password, for the admin page. The flag says whether
/// a password is set.
pub async fn describe(pool: &DbPool) -> Result<Option<(SmtpConfig, Source, bool)>, String> {
    match db::email::get_email_settings(pool)
        .await
        .map_err(unavailable)?
    {
        Some(row) => {
            let password_set = row.encrypted_password.is_some();
            Ok(Some((from_row(row, None)?, Source::Database, password_set)))
        }
        None => Ok(from_env()?.map(|config| {
            let password_set = config.password.is_some();
            (config, Source::Environment, password_set)
        })),
    }
}

/// Whether a mail server is set up, without opening the password.
pub async fn is_configured(pool: &DbPool) -> bool {
    match db::email::get_email_settings(pool).await {
        Ok(Some(_)) => true,
        Ok(None) => matches!(from_env(), Ok(Some(_))),
        Err(error) => {
            tracing::warn!(%error, "could not read email settings");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
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
}
