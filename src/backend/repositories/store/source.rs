//! Fetch every advertised ref through Git's smart HTTP protocol, without following upstream URLs.
use crate::backend::{
    repositories::imports::{self, Credentials, MAX_IMPORT_PACK_BYTES},
    webhooks::outbound,
};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{header, Client, Response};
use std::{collections::HashMap, time::Duration};
use url::Url;

const MAX_ADVERTISEMENT_BYTES: usize = 8 * 1024 * 1024;

fn packet(input: &mut &[u8]) -> Result<Option<Vec<u8>>> {
    ensure!(input.len() >= 4, "Truncated Git packet");
    let size = usize::from_str_radix(std::str::from_utf8(&input[..4])?, 16)?;
    *input = &input[4..];
    if size == 0 {
        return Ok(None);
    }
    ensure!(
        (4..=65520).contains(&size) && input.len() >= size - 4,
        "Invalid Git packet"
    );
    let payload = input[..size - 4].to_vec();
    *input = &input[size - 4..];
    Ok(Some(payload))
}

pub(crate) fn append_packet(output: &mut Vec<u8>, line: &str) {
    output.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
    output.extend_from_slice(line.as_bytes());
}

async fn bytes(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response.status().is_success(),
        "Git source refused the request"
    );
    let mut output = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= limit - output.len(),
            "Git source exceeds import size limit"
        );
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn endpoint(source: &Url, suffix: &str) -> Url {
    let mut url = source.clone();
    url.set_path(&format!("{}/{suffix}", source.path().trim_end_matches('/')));
    url
}

fn request(
    client: &Client,
    method: reqwest::Method,
    url: Url,
    credentials: Option<&Credentials>,
) -> reqwest::RequestBuilder {
    let request = client.request(method, url);
    if let Some(credentials) = credentials {
        request.basic_auth(&credentials.username, Some(&credentials.password))
    } else {
        request
    }
}

fn advertisement(mut input: &[u8]) -> Result<(HashMap<String, String>, Option<String>)> {
    ensure!(
        packet(&mut input)?.as_deref() == Some(b"# service=git-upload-pack\n"),
        "Not a Git smart HTTP source"
    );
    ensure!(packet(&mut input)?.is_none(), "Invalid Git advertisement");
    let mut refs = HashMap::new();
    let mut default_branch = None;
    let mut head_sha = None;
    let mut first = true;
    while let Some(line) = packet(&mut input)? {
        let line = std::str::from_utf8(&line)?.trim_end_matches('\n');
        let (reference, capabilities) = line.split_once('\0').unwrap_or((line, ""));
        if first {
            ensure!(
                capabilities
                    .split_whitespace()
                    .any(|word| word == "side-band-64k"),
                "Git source does not support sideband transfer"
            );
            default_branch = capabilities.split_whitespace().find_map(|word| {
                word.strip_prefix("symref=HEAD:refs/heads/")
                    .map(str::to_owned)
            });
            first = false;
        }
        let (sha, name) = reference.split_once(' ').context("Invalid Git ref")?;
        if name == "HEAD" {
            head_sha = Some(sha.to_owned());
        }
        if name == "HEAD" || name.ends_with("^{}") || name == "capabilities^{}" {
            continue;
        }
        ensure!(
            crate::backend::git::is_full_commit_id(sha)
                && name.starts_with("refs/")
                && git2::Reference::is_valid_name(name),
            "Invalid Git ref"
        );
        // Server-owned provenance cannot be adopted from an untrusted source, even
        // when that server mistakenly advertises its internal namespace.
        if name == "refs/codeyard" || name.starts_with("refs/codeyard/") {
            continue;
        }
        ensure!(
            refs.insert(name.to_owned(), sha.to_owned()).is_none(),
            "Duplicate Git ref"
        );
        ensure!(
            refs.len() <= crate::backend::git::MAX_REPOSITORY_REFS,
            "Too many Git refs"
        );
    }
    ensure!(input.is_empty(), "Unexpected Git advertisement data");
    ensure!(!refs.is_empty() || head_sha.is_none(), "Detached HEAD without named refs cannot be preserved by an import");
    // Without the symref capability, the default is a branch at the commit that HEAD names.
    if default_branch.is_none() {
        default_branch = head_sha.and_then(|head| {
            refs.iter()
                .filter(|(_, sha)| **sha == head)
                .filter_map(|(name, _)| name.strip_prefix("refs/heads/"))
                .min()
                .map(str::to_owned)
        });
    }
    Ok((refs, default_branch))
}

fn unpack(mut input: Vec<u8>) -> Result<Vec<u8>> {
    let mut read = 0;
    let mut written = 0;
    let mut acknowledged = false;
    while read < input.len() {
        ensure!(input.len() - read >= 4, "Truncated Git packet");
        let size = usize::from_str_radix(std::str::from_utf8(&input[read..read + 4])?, 16)?;
        read += 4;
        if size == 0 { continue; }
        ensure!((4..=65520).contains(&size) && input.len() - read >= size - 4, "Invalid Git packet");
        let end = read + size - 4;
        if !acknowledged {
            ensure!(&input[read..end] == b"NAK\n", "Invalid Git acknowledgement");
            acknowledged = true;
        } else {
            match input.get(read).filter(|_| read < end) {
                Some(1) => {
                    let length = end - read - 1;
                    ensure!(length <= MAX_IMPORT_PACK_BYTES - written, "Git pack exceeds import size limit");
                    input.copy_within(read + 1..end, written);
                    written += length;
                }
                Some(2) => {}
                _ => bail!("Git source transfer failed"),
            }
        }
        read = end;
    }
    input.truncate(written);
    ensure!(input.starts_with(b"PACK"), "Missing Git pack");
    Ok(input)
}

pub(crate) async fn fetch(
    source: &str,
    credentials: Option<Credentials>,
) -> Result<(HashMap<String, String>, Option<String>, Vec<u8>)> {
    let url = imports::source_url(source)?;
    let addresses = tokio::time::timeout(
        Duration::from_secs(15),
        outbound::resolve(&url, imports::allow_private()),
    )
    .await?
    .map_err(|_| anyhow::anyhow!("Source address refused"))?;
    let client = outbound::pinned_client(&url, &addresses)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .user_agent("Codeyard repository importer")
        .build()?;
    let mut discovery = endpoint(&url, "info/refs");
    discovery.set_query(Some("service=git-upload-pack"));
    let response = request(
        &client,
        reqwest::Method::GET,
        discovery,
        credentials.as_ref(),
    )
    .send()
    .await?;
    ensure!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(';').next()
                == Some("application/x-git-upload-pack-advertisement")),
        "Not a Git smart HTTP source"
    );
    let (refs, default_branch) = advertisement(&bytes(response, MAX_ADVERTISEMENT_BYTES).await?)?;
    if refs.is_empty() {
        return Ok((refs, default_branch, Vec::new()));
    }
    let mut wants: Vec<_> = refs.values().collect();
    wants.sort_unstable();
    wants.dedup();
    let mut body = Vec::new();
    for (index, sha) in wants.iter().enumerate() {
        append_packet(
            &mut body,
            &format!(
                "want {sha}{}\n",
                if index == 0 {
                    " side-band-64k"
                } else {
                    ""
                }
            ),
        );
    }
    body.extend_from_slice(b"0000");
    append_packet(&mut body, "done\n");
    let response = request(
        &client,
        reqwest::Method::POST,
        endpoint(&url, "git-upload-pack"),
        credentials.as_ref(),
    )
    .header(
        header::CONTENT_TYPE,
        "application/x-git-upload-pack-request",
    )
    .header(header::ACCEPT, "application/x-git-upload-pack-result")
    .body(body)
    .send()
    .await?;
    ensure!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(
                |value| value.split(';').next() == Some("application/x-git-upload-pack-result")
            ),
        "Invalid Git transfer type"
    );
    let pack = unpack(bytes(response, MAX_IMPORT_PACK_BYTES + 8 * 1024 * 1024).await?)?;
    Ok((refs, default_branch, pack))
}

#[cfg(test)]
mod tests;
