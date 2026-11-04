//! Registry downloads verify patch candidates; advisory matching belongs to vulnerabilities.
use anyhow::Context;

use anyhow::ensure;

use anyhow::Result;
use semver::Version;
use serde_json::Value;
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    registry: Url,
}
fn trusted_origin(origin: &str) -> Result<Url> {
    let url = Url::parse(origin)?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    ensure!(
        (url.scheme() == "https" || url.scheme() == "http" && local)
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/'),
        "Invalid trusted registry origin"
    );
    Ok(url)
}
impl Client {
    /// Only operator-configured origins, never URLs supplied by a repository.
    pub fn new(origin: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("Codeyard/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            registry: trusted_origin(origin)?,
        })
    }
    fn archive_url(&self, official: &str) -> Result<Url> {
        ensure!(
            codeyard_repos::dependencies::npm::registry_url(official),
            "Untrusted npm archive URL"
        );
        let source = Url::parse(official)?;
        Ok(self
            .registry
            .join(&format!("./{}", source.path().trim_start_matches('/')))?)
    }
    async fn body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
        let status = response.status();
        // A busy or failing registry is retried like a network failure, so its error stays a
        // `reqwest::Error`. Other refusals are final.
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
        {
            if let Err(error) = response.error_for_status_ref() {
                return Err(error.into());
            }
        }
        ensure!(status.is_success(), "Registry request failed");
        ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "Registry response is too large"
        );
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_some_and(|n| n <= limit),
                "Registry response is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    pub async fn archive(
        &self,
        name: &str,
        version: &Version,
        expected: Option<&str>,
    ) -> Result<codeyard_repos::dependencies::npm::Archive> {
        ensure!(
            codeyard_repos::npm::dependency_name(name) && version.to_string().len() <= 256,
            "Invalid npm package request"
        );
        let mut url = self.registry.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid npm origin"))?
            .pop_if_empty()
            .push(name)
            .push(&version.to_string());
        let bytes = Self::body(self.http.get(url).send().await?, 256 * 1024).await?;
        let metadata: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            metadata.get("name").and_then(Value::as_str) == Some(name)
                && metadata.get("version").and_then(Value::as_str)
                    == Some(version.to_string().as_str()),
            "Npm metadata identity mismatch"
        );
        let tarball = metadata
            .pointer("/dist/tarball")
            .and_then(Value::as_str)
            .context("Missing npm archive URL")?;
        ensure!(
            codeyard_repos::dependencies::npm::registry_url(tarball),
            "Untrusted npm archive URL"
        );
        let bytes = Self::body(
            self.http.get(self.archive_url(tarball)?).send().await?,
            codeyard_repos::dependencies::npm::MAX_ARCHIVE,
        )
        .await?;
        let expected = expected.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            codeyard_repos::dependencies::npm::Archive::verify(
                &metadata,
                &bytes,
                expected.as_deref(),
            )
        })
        .await?
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_mirrors_preserve_official_source_validation() {
        assert!(Client::new("http://untrusted.example/").is_err());
        assert!(Client::new("https://user:secret@example.test/").is_err());
        let client = Client::new("http://127.0.0.1:2/npm/").unwrap();
        assert_eq!(
            client
                .archive_url("https://registry.npmjs.org/@scope/foo/-/foo-1.2.3.tgz")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:2/npm/@scope/foo/-/foo-1.2.3.tgz"
        );
        assert!(client.archive_url("https://evil.test/foo.tgz").is_err());
    }
}
