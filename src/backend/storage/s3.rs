//! Uploaded files in an S3-compatible bucket. Clients upload and download
//! directly with presigned URLs; the server only signs, checks and deletes.
use super::{media_signature_matches, valid_key, StorageError, Upload, MAX_OBJECT_BYTES};
use anyhow::{anyhow, ensure, Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    config::{BehaviorVersion, Builder as S3ConfigBuilder, Region},
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{BucketLocationConstraint, CreateBucketConfiguration},
    Client,
};
use aws_smithy_types::timeout::TimeoutConfig;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, net::IpAddr, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub public_endpoint: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    pub bucket: String,
    pub region: String,
}

#[derive(Clone, PartialEq, Eq)]
enum ConfiguredHost {
    Domain(String),
    Ip(IpAddr),
}

impl ConfiguredHost {
    fn from_url(parsed: &url::Url) -> Option<Self> {
        match parsed.host()? {
            url::Host::Domain(domain) => Some(Self::Domain(
                domain.trim_end_matches('.').to_ascii_lowercase(),
            )),
            url::Host::Ipv4(address) => Some(Self::Ip(address.into())),
            url::Host::Ipv6(address) => Some(Self::Ip(address.into())),
        }
    }

    fn from_cookie_domain(value: &str) -> Result<Self> {
        let value = value.trim();
        let value = value.strip_prefix('.').unwrap_or(value);
        ensure!(
            !value.is_empty() && !value.starts_with('.') && !value.ends_with('.'),
            "Invalid COOKIE_DOMAIN"
        );
        match url::Host::parse(value).map_err(|_| anyhow!("Invalid COOKIE_DOMAIN"))? {
            url::Host::Domain(domain) => Ok(Self::Domain(domain.to_ascii_lowercase())),
            url::Host::Ipv4(address) => Ok(Self::Ip(address.into())),
            url::Host::Ipv6(address) => Ok(Self::Ip(address.into())),
        }
    }

    fn is_within(&self, cookie_domain: &Self) -> bool {
        match (self, cookie_domain) {
            (Self::Domain(host), Self::Domain(domain)) => {
                host == domain
                    || host
                        .strip_suffix(domain.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
            (Self::Ip(host), Self::Ip(domain)) => host == domain,
            _ => false,
        }
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(anyhow!("Invalid browser storage configuration: {name}"))
        }
    }
}

fn public_url_host(value: &str) -> Result<ConfiguredHost> {
    let url = url::Url::parse(value).map_err(|_| anyhow!("Invalid PUBLIC_URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Invalid PUBLIC_URL"
    );
    ConfiguredHost::from_url(&url).ok_or_else(|| anyhow!("Invalid PUBLIC_URL"))
}

impl S3Config {
    pub fn from_env() -> Result<Self> {
        let required = |name| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Required S3 configuration is absent: {name}"))
        };
        let optional = |name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(anyhow!("Invalid S3 configuration: {name}")),
        };
        let config = Self {
            endpoint: required("S3_ENDPOINT")?,
            public_endpoint: optional("S3_PUBLIC_ENDPOINT")?,
            access_key: required("S3_ACCESS_KEY")?,
            secret_key: required("S3_SECRET_KEY")?,
            bucket: required("S3_BUCKET")?,
            region: optional("S3_REGION")?.unwrap_or_else(|| "us-east-1".into()),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn get_public_endpoint(&self) -> &str {
        self.public_endpoint.as_deref().unwrap_or(&self.endpoint)
    }

    pub fn validate(&self) -> Result<()> {
        for value in [self.endpoint.as_str(), self.get_public_endpoint()] {
            ensure!(value.len() <= 2048, "Invalid S3 endpoint");
            let url = url::Url::parse(value).map_err(|_| anyhow!("Invalid S3 endpoint"))?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "Invalid S3 endpoint"
            );
        }
        ensure!(
            (1..=1024).contains(&self.access_key.len())
                && (1..=8192).contains(&self.secret_key.len())
                && (1..=255).contains(&self.bucket.len())
                && (1..=100).contains(&self.region.len()),
            "Incomplete S3 configuration"
        );
        Ok(())
    }

    /// Direct browser storage must be outside the host/domain that receives the
    /// session cookie. Schemes and ports do not provide cookie isolation.
    pub fn validate_browser_cookie_scope(
        &self,
        public_url: Option<&str>,
        cookie_domain: Option<&str>,
    ) -> Result<()> {
        self.validate()?;
        let public_url = public_url
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("PUBLIC_URL is required when S3 serves browser uploads"))?;
        let forge_host = public_url_host(public_url)?;
        let endpoint_url = url::Url::parse(self.get_public_endpoint())
            .map_err(|_| anyhow!("Invalid S3 endpoint"))?;
        let endpoint_host = ConfiguredHost::from_url(&endpoint_url)
            .ok_or_else(|| anyhow!("Invalid S3 endpoint"))?;

        let overlaps = match cookie_domain
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => {
                let domain = ConfiguredHost::from_cookie_domain(value)?;
                ensure!(
                    forge_host.is_within(&domain),
                    "COOKIE_DOMAIN does not include the PUBLIC_URL host"
                );
                endpoint_host.is_within(&domain)
            }
            None => endpoint_host == forge_host,
        };
        ensure!(
            !overlaps,
            "S3_PUBLIC_ENDPOINT must use a host outside the session cookie scope"
        );
        Ok(())
    }

    pub fn validate_browser_cookie_scope_from_env(&self) -> Result<()> {
        let public_url = optional_env("PUBLIC_URL")?;
        let cookie_domain = optional_env("COOKIE_DOMAIN")?;
        self.validate_browser_cookie_scope(public_url.as_deref(), cookie_domain.as_deref())
    }
}

pub struct S3Store {
    client: Client,
    /// Signs URLs against S3_PUBLIC_ENDPOINT, the address browsers can reach.
    signing_client: Client,
    config: S3Config,
}

/// A presigned upload; the client must send exactly these headers.
pub struct PresignedUpload {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

impl S3Store {
    pub async fn open(config: S3Config) -> Result<Self> {
        config.validate()?;
        let client = create_client(&config).await?;
        let mut public = config.clone();
        public.endpoint = config.get_public_endpoint().to_owned();
        let signing_client = create_client(&public).await?;
        Ok(Self {
            client,
            signing_client,
            config,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.config.bucket
    }

    /// Creates the bucket when it does not exist: a new RustFS or MinIO starts without one.
    pub async fn ensure_bucket(&self) -> Result<()> {
        match self.client.head_bucket().bucket(self.bucket()).send().await {
            Ok(_) => return Ok(()),
            Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => {}
            Err(error) => return Err(anyhow::Error::new(error).context("check the S3 bucket")),
        }
        let mut create = self.client.create_bucket().bucket(self.bucket());
        if self.config.region != "us-east-1" {
            create = create.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(
                        self.config.region.as_str(),
                    ))
                    .build(),
            );
        }
        match create.send().await {
            Ok(_) => Ok(()),
            // Another node created it first.
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|e| e.is_bucket_already_owned_by_you()) =>
            {
                Ok(())
            }
            Err(error) => Err(anyhow::Error::new(error).context("create the S3 bucket")),
        }
    }

    /// Size, type and SHA-256 are signed into the URL, so S3 rejects any other
    /// bytes; `If-None-Match` stops a second upload from replacing the first.
    pub async fn presigned_put(
        &self,
        upload: &Upload<'_>,
        expires_in: Duration,
    ) -> Result<PresignedUpload> {
        ensure!(valid_key(upload.key), "invalid object key");
        ensure!(
            (1..=MAX_OBJECT_BYTES).contains(&upload.bytes),
            "invalid upload size"
        );
        let checksum = checksum_base64(upload.sha256_hex)?;
        let request = self
            .signing_client
            .put_object()
            .bucket(self.bucket())
            .key(upload.key)
            .content_length(upload.bytes)
            .content_type(upload.content_type)
            .checksum_sha256(&checksum)
            .if_none_match("*")
            .presigned(PresigningConfig::expires_in(expires_in)?)
            .await?;
        let url = request.uri().to_string();
        let parsed = url::Url::parse(&url)?;
        let signed = parsed
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-amz-signedheaders"))
            .map(|(_, v)| v.into_owned())
            .context("missing signed headers")?;
        for header in ["content-length", "content-type", "if-none-match"] {
            ensure!(
                signed.split(';').any(|h| h == header),
                "upload constraint was not signed"
            );
        }
        let query_checksum = parsed.query_pairs().any(|(k, v)| {
            k.eq_ignore_ascii_case("x-amz-checksum-sha256") && v.as_ref() == checksum
        });
        ensure!(
            query_checksum || signed.split(';').any(|h| h == "x-amz-checksum-sha256"),
            "upload checksum was not signed"
        );
        let mut headers: BTreeMap<String, String> = request
            .headers()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        headers.insert("content-length".into(), upload.bytes.to_string());
        headers.insert("content-type".into(), upload.content_type.into());
        headers.insert("if-none-match".into(), "*".into());
        if !query_checksum {
            headers.insert("x-amz-checksum-sha256".into(), checksum);
        }
        Ok(PresignedUpload { url, headers })
    }

    /// Uploads bytes this server holds itself, with the same constraints a presigned
    /// upload carries: size, type and SHA-256, and no replacing an existing object.
    pub async fn put_object(&self, upload: &Upload<'_>, bytes: Vec<u8>) -> Result<()> {
        ensure!(valid_key(upload.key), "invalid object key");
        ensure!(
            (1..=MAX_OBJECT_BYTES).contains(&upload.bytes),
            "invalid upload size"
        );
        ensure!(
            bytes.len() as i64 == upload.bytes,
            "upload is not the declared size"
        );
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == upload.sha256_hex,
            "upload checksum does not match"
        );
        ensure!(
            media_signature_matches(upload.content_type, &bytes),
            "upload content does not match its type"
        );
        self.client
            .put_object()
            .bucket(self.bucket())
            .key(upload.key)
            .content_length(upload.bytes)
            .content_type(upload.content_type)
            .checksum_sha256(checksum_base64(upload.sha256_hex)?)
            .if_none_match("*")
            .body(ByteStream::from(bytes))
            .send()
            .await?;
        Ok(())
    }

    /// Only bitmap images may display inline; everything else is an attachment.
    pub async fn presigned_get(
        &self,
        key: &str,
        filename: &str,
        seconds: u64,
        inline_image: Option<&str>,
    ) -> Result<String> {
        ensure!(valid_key(key), "invalid object key");
        if let Some(kind) = inline_image {
            ensure!(
                matches!(
                    kind,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ),
                "unsafe inline object type"
            );
        }
        let request = self
            .signing_client
            .get_object()
            .bucket(self.bucket())
            .key(key)
            .response_content_type(inline_image.unwrap_or("application/octet-stream"))
            .response_content_disposition(if inline_image.is_some() {
                "inline".to_owned()
            } else {
                super::attachment_disposition(filename)
            })
            .presigned(PresigningConfig::expires_in(Duration::from_secs(seconds))?)
            .await?;
        Ok(request.uri().to_string())
    }

    /// S3 already checked the checksum; confirm the size and file signature.
    pub async fn verify(&self, upload: &Upload<'_>) -> Result<(), StorageError> {
        if !valid_key(upload.key) {
            return Err(StorageError::Invalid);
        }
        let head = self
            .client
            .head_object()
            .bucket(self.bucket())
            .key(upload.key)
            .send()
            .await
            .map_err(|error| {
                if error
                    .as_service_error()
                    .is_some_and(|error| error.is_not_found())
                {
                    StorageError::NotFound
                } else {
                    StorageError::Unavailable
                }
            })?;
        if head.content_length() != Some(upload.bytes) {
            return Err(StorageError::Rejected);
        }
        // A type without a signature check accepts any bytes, so there is nothing to read.
        if media_signature_matches(upload.content_type, &[]) {
            return Ok(());
        }
        let object = self
            .client
            .get_object()
            .bucket(self.bucket())
            .key(upload.key)
            .range("bytes=0-15")
            .send()
            .await
            .map_err(|_| StorageError::Unavailable)?;
        let mut prefix = Vec::with_capacity(16);
        object
            .body
            .into_async_read()
            .take(16)
            .read_to_end(&mut prefix)
            .await?;
        if !media_signature_matches(upload.content_type, &prefix) {
            return Err(StorageError::Rejected);
        }
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        ensure!(valid_key(key), "invalid object key");
        self.client
            .delete_object()
            .bucket(self.bucket())
            .key(key)
            .send()
            .await?;
        Ok(())
    }
}

fn checksum_base64(hex_value: &str) -> Result<String> {
    let digest = hex::decode(hex_value).map_err(|_| anyhow!("Invalid upload checksum"))?;
    ensure!(digest.len() == 32, "Invalid upload checksum");
    Ok(STANDARD.encode(digest))
}

pub async fn create_client(config: &S3Config) -> Result<Client> {
    config.validate()?;
    let credentials = Credentials::new(
        &config.access_key,
        &config.secret_key,
        None,
        None,
        "explicit-config",
    );
    let config = S3ConfigBuilder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(&config.endpoint)
        .region(Region::new(config.region.clone()))
        .credentials_provider(credentials)
        .force_path_style(true)
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(5))
                .operation_timeout(Duration::from_secs(60))
                .build(),
        )
        .build();
    Ok(Client::from_conf(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_encoding_is_exact() {
        assert_eq!(
            checksum_base64(&"00".repeat(32)).unwrap(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
        assert!(checksum_base64("not-a-digest").is_err());
        assert!(checksum_base64(&"00".repeat(31)).is_err());
    }
}
