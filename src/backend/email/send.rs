//! Building a message and handing it to the SMTP server.
use super::config::{Security, SmtpConfig};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::str::FromStr;
use std::time::Duration;

fn address(text: &str) -> Result<Address, String> {
    Address::from_str(text).map_err(|_| format!("{text} is not a valid email address."))
}

pub fn build_message(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<Message, String> {
    let from_name = (!config.from_name.is_empty()).then(|| config.from_name.clone());
    let mut builder = Message::builder()
        .from(Mailbox::new(from_name, address(&config.from_address)?))
        .to(Mailbox::new(None, address(to)?))
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);
    if !config.reply_to.is_empty() {
        builder = builder.reply_to(Mailbox::new(None, address(&config.reply_to)?));
    }
    builder
        .body(body.to_owned())
        .map_err(|error| error.to_string())
}

fn transport(config: &SmtpConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let builder = match config.security {
        Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host),
        Security::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host),
        Security::Plain => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            &config.host,
        )),
    }
    .map_err(|error| error.to_string())?;
    let mut builder = builder
        .port(config.port)
        .timeout(Some(Duration::from_secs(30)));
    if !config.username.is_empty() {
        builder = builder.credentials(Credentials::new(
            config.username.clone(),
            config.password.clone().unwrap_or_default(),
        ));
    }
    Ok(builder.build())
}

/// Sends one message and waits for the server's answer. The error is the server's or the
/// connection's own text, kept short.
pub async fn send(config: &SmtpConfig, to: &str, subject: &str, body: &str) -> Result<(), String> {
    let message = build_message(config, to, subject, body)?;
    transport(config)?
        .send(message)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string().chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn config(port: u16) -> SmtpConfig {
        SmtpConfig {
            host: "127.0.0.1".into(),
            port,
            security: Security::Plain,
            username: String::new(),
            password: None,
            from_name: "Codeyard".into(),
            from_address: "noreply@example.com".into(),
            reply_to: "help@example.com".into(),
        }
    }

    #[test]
    fn the_message_carries_the_configured_headers() {
        let message = build_message(&config(25), "bob@example.com", "Hello", "Body text").unwrap();
        let text = String::from_utf8(message.formatted()).unwrap();
        assert!(
            text.contains("From: Codeyard <noreply@example.com>"),
            "{text}"
        );
        assert!(text.contains("To: bob@example.com"), "{text}");
        assert!(text.contains("Reply-To: help@example.com"), "{text}");
        assert!(text.contains("Subject: Hello"), "{text}");
        assert!(text.contains("Body text"), "{text}");
        assert!(build_message(&config(25), "not an address", "Hello", "Body").is_err());
    }

    /// A one-message SMTP server that answers every command and returns what it was sent.
    async fn fake_server() -> (u16, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut received = String::new();
            let mut in_data = false;
            write.write_all(b"220 fake ready\r\n").await.unwrap();
            while let Some(line) = lines.next_line().await.unwrap() {
                if in_data {
                    if line == "." {
                        in_data = false;
                        write.write_all(b"250 queued\r\n").await.unwrap();
                    } else {
                        received.push_str(&line);
                        received.push('\n');
                    }
                    continue;
                }
                let reply: &[u8] = match line.split(' ').next().unwrap_or("") {
                    "EHLO" => b"250 fake\r\n",
                    "DATA" => {
                        in_data = true;
                        b"354 go\r\n"
                    }
                    "QUIT" => {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        break;
                    }
                    _ => b"250 ok\r\n",
                };
                write.write_all(reply).await.unwrap();
            }
            received
        });
        (port, task)
    }

    #[tokio::test]
    async fn a_message_reaches_an_smtp_server() {
        let (port, server) = fake_server().await;
        send(&config(port), "bob@example.com", "Hello", "Body text")
            .await
            .unwrap();
        let received = server.await.unwrap();
        assert!(received.contains("Subject: Hello"), "{received}");
        assert!(received.contains("Body text"), "{received}");
    }

    #[tokio::test]
    async fn a_refused_connection_is_reported() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(send(&config(port), "bob@example.com", "Hello", "Body")
            .await
            .is_err());
    }
}
