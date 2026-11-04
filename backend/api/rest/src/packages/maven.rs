//! Maven repository-layout requests share a host with repository pages and Git.
use crate::packages::authenticate;
use crate::packages::Error;
use crate::packages::Result;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::Method;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_app::app::AppState;
use codeyard_db_repos::packages::MavenFile;
use codeyard_db_repos::packages::Permission;
use codeyard_support::object_limits::MAX_OBJECT_BYTES;
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;

struct Artifact {
    owner: String,
    repo: String,
    name: String,
    path: String,
    version: Option<String>,
    checksum: Option<String>,
    metadata: bool,
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

fn artifact(path: &str) -> Option<Artifact> {
    let parts: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(codeyard_webui::decode_component)
        .collect();
    if parts.len() < 4
        || parts
            .iter()
            .any(|s| s.is_empty() || s == "." || s == ".." || s.contains('/') || s.contains('\\'))
    {
        return None;
    }
    if parts[2..].iter().any(|s| {
        !s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
    }) {
        return None;
    }
    // These are application namespaces, never Maven owners.
    if matches!(parts[0].as_str(), "api" | "assets" | "wasm" | "v2") {
        return None;
    }
    let mut filename = parts.last()?.clone();
    let checksum = ["sha1", "sha256", "sha512", "md5"]
        .into_iter()
        .find(|ext| filename.ends_with(&format!(".{ext}")))
        .map(str::to_owned);
    if let Some(ext) = &checksum {
        filename.truncate(filename.len() - ext.len() - 1);
    }
    let metadata = filename == "maven-metadata.xml";
    let (artifact_index, version) = if metadata {
        // Version-level SNAPSHOT metadata sits below the artifact; release metadata does not.
        if parts.len() >= 6 && parts[parts.len() - 2].ends_with("-SNAPSHOT") {
            (parts.len() - 3, Some(parts[parts.len() - 2].clone()))
        } else {
            (parts.len() - 2, None)
        }
    } else {
        if parts.len() < 6 {
            return None;
        }
        let index = parts.len() - 3;
        let version = &parts[index + 1];
        let prefix = format!(
            "{}-{}",
            parts[index],
            version.strip_suffix("-SNAPSHOT").unwrap_or(version)
        );
        let suffix = filename.strip_prefix(&prefix)?;
        if !(suffix.starts_with('.') || suffix.starts_with('-')) {
            return None;
        }
        (index, Some(version.clone()))
    };
    if artifact_index < 2 || (!metadata && artifact_index < 3) {
        return None;
    }
    let group = parts[2..artifact_index].join(".");
    let name = if artifact_index == 2 {
        String::new()
    } else {
        format!("{group}:{}", parts[artifact_index])
    };
    if (!name.is_empty() && !codeyard_app::packages::valid_package_name("maven", &name))
        || version.as_ref().is_some_and(|v| !valid_version(v))
    {
        return None;
    }
    let mut relative = parts[2..parts.len() - 1].join("/");
    relative.push('/');
    relative.push_str(&filename);
    // package_files indexes the complete relative path on both database engines.
    // Reject unsupported paths before dispatch can persist their bytes.
    if relative.len() > 1024 {
        return None;
    }
    Some(Artifact {
        owner: parts[0].clone(),
        repo: parts[1].clone(),
        name,
        path: relative,
        version,
        checksum,
        metadata,
    })
}

// This middleware wraps every application route. Keep registry futures off the
// caller's stack, including when a request simply proceeds to a native handler.
pub(crate) fn route(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> futures_util::future::BoxFuture<'static, Response> {
    Box::pin(async move {
        let native_page = request
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .is_some();
        let agent = request
            .headers()
            .get(header::USER_AGENT)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // A browser viewing a JAR in a Git tree must not become a registry request.
        // Maven/Gradle can also use group IDs colliding with native route prefixes.
        if native_page
            && request.method() != Method::PUT
            && !agent.contains("maven")
            && !agent.contains("gradle")
        {
            return next.run(request).await;
        }
        let Some(artifact) = artifact(request.uri().path()) else {
            return next.run(request).await;
        };
        if !matches!(*request.method(), Method::GET | Method::HEAD | Method::PUT) {
            return next.run(request).await;
        }
        let anonymous = !request.headers().contains_key(header::AUTHORIZATION);
        crate::packages::finish(
            tokio::time::timeout(
                Duration::from_secs(120),
                dispatch(&state, artifact, request),
            )
            .await,
            anonymous,
            crate::packages::RegistryFormat::Standard,
        )
    })
}

fn checksum(algorithm: &str, bytes: &[u8]) -> String {
    match algorithm {
        "sha1" => hex::encode(sha1::Sha1::digest(bytes)),
        "sha256" => hex::encode(sha2::Sha256::digest(bytes)),
        "sha512" => hex::encode(sha2::Sha512::digest(bytes)),
        _ => hex::encode(md5::Md5::digest(bytes)),
    }
}

fn dispatch(
    state: &AppState,
    mut artifact: Artifact,
    request: Request,
) -> futures_util::future::BoxFuture<'_, Result<Response>> {
    Box::pin(async move {
        let write = request.method() == Method::PUT;
        let auth = authenticate(state, request.headers(), write).await?;
        let owner = crate::packages::owner(state, &artifact.owner).await?;
        let repo = codeyard_db_repos::repositories::records::get_repository(
            &state.pool,
            &artifact.owner,
            &artifact.repo,
        )
        .await
        .map_err(Error::db)?
        .ok_or_else(Error::missing)?;
        let token = auth.as_ref().map(|a| a.token.id.into());
        if !write {
            let (_, blob) = codeyard_db_repos::packages::maven_file(
                &state.pool,
                repo.id,
                token,
                &artifact.path,
            )
            .await
            .map_err(Error::db)?;
            if let Some(algorithm) = &artifact.checksum {
                let length = match algorithm.as_str() {
                    "sha1" => 40,
                    "sha256" => 64,
                    "sha512" => 128,
                    _ => 32,
                };
                if request.method() == Method::HEAD {
                    return Ok((
                        [
                            (header::CONTENT_TYPE, "text/plain"),
                            (header::CONTENT_LENGTH, &length.to_string()),
                        ],
                        "",
                    )
                        .into_response());
                }
                let _slot = state
                    .storage
                    .try_acquire_download()
                    .ok_or_else(Error::unavailable)?;
                let bytes = codeyard_app::packages::read(&state.storage, &blob)
                    .await
                    .map_err(Error::db)?;
                let digest = checksum(algorithm, &bytes);
                let length = digest.len();
                return Ok((
                    [
                        (header::CONTENT_TYPE, "text/plain"),
                        (header::CONTENT_LENGTH, &length.to_string()),
                    ],
                    if request.method() == Method::HEAD {
                        String::new()
                    } else {
                        digest
                    },
                )
                    .into_response());
            }
            let mut response =
                crate::packages::oci::download(state, &blob, request.method(), request.headers())
                    .await?;
            if artifact.path.ends_with(".pom") || artifact.metadata {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
                response.headers_mut().insert(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; sandbox".parse().unwrap(),
                );
            }
            return Ok(response);
        }
        let auth = crate::packages::writer(auth.as_ref())?;
        let _slot = crate::packages::upload_slot(state)?;
        let bytes = to_bytes(
            request.into_body(),
            if artifact.checksum.is_some() {
                1024
            } else {
                MAX_OBJECT_BYTES as usize
            },
        )
        .await
        .map_err(|_| {
            Error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "Package exceeds size limit",
            )
        })?;
        if let Some(algorithm) = artifact.checksum {
            let (package, blob) = codeyard_db_repos::packages::maven_file(
                &state.pool,
                repo.id,
                Some(auth.token.id.into()),
                &artifact.path,
            )
            .await
            .map_err(Error::db)?;
            codeyard_db_repos::packages::find(
                &state.pool,
                &owner,
                "maven",
                &package.name,
                Some(auth.token.id.into()),
                Permission::Write,
                false,
            )
            .await
            .map_err(Error::db)?;
            let content = codeyard_app::packages::read(&state.storage, &blob)
                .await
                .map_err(Error::db)?;
            if !std::str::from_utf8(&bytes)
                .map_err(|_| Error::invalid())?
                .trim()
                .eq_ignore_ascii_case(&checksum(&algorithm, &content))
            {
                return Err(Error::invalid());
            }
            return Ok(StatusCode::CREATED.into_response());
        }
        let mut plugin_metadata = false;
        if artifact.metadata || artifact.path.ends_with(".pom") {
            let xml = crate::packages::archives::xml(&bytes)?;
            if xml.name
                != if artifact.metadata {
                    "metadata"
                } else {
                    "project"
                }
            {
                return Err(Error::invalid());
            }
            if artifact.metadata {
                let parent = artifact
                    .path
                    .strip_suffix("/maven-metadata.xml")
                    .ok_or_else(Error::invalid)?;
                let group = xml.value("groupId");
                let group_path = group.replace('.', "/");
                if let Some(plugins) = xml.child("plugins") {
                    let name = plugins
                        .children
                        .iter()
                        .find(|p| p.name == "plugin")
                        .map(|p| p.value("artifactId"))
                        .filter(|n| !n.is_empty())
                        .ok_or_else(Error::invalid)?;
                    let group = if group.is_empty() {
                        parent.replace('/', ".")
                    } else {
                        group.to_owned()
                    };
                    if parent != group.replace('.', "/") {
                        return Err(Error::invalid());
                    }
                    artifact.name = format!("{group}:{name}");
                    artifact.version = None;
                    plugin_metadata = true;
                } else {
                    let name = xml.value("artifactId");
                    let prefix = format!("{group_path}/{name}");
                    artifact.name = format!("{group}:{name}");
                    artifact.version = if parent == prefix {
                        None
                    } else {
                        let version = parent
                            .strip_prefix(&format!("{prefix}/"))
                            .filter(|v| {
                                !v.is_empty() && *v == xml.value("version") && !v.contains('/')
                            })
                            .ok_or_else(Error::invalid)?;
                        Some(version.to_owned())
                    };
                }
            }
        }
        if !codeyard_app::packages::valid_package_name("maven", &artifact.name)
            || artifact.version.as_ref().is_some_and(|v| !valid_version(v))
        {
            return Err(Error::invalid());
        }
        let package = if plugin_metadata {
            codeyard_db_repos::packages::find(
                &state.pool,
                &owner,
                "maven",
                &artifact.name,
                Some(auth.token.id.into()),
                Permission::Write,
                false,
            )
            .await
            .map_err(Error::db)?
        } else {
            codeyard_db_repos::packages::create(
                &state.pool,
                &owner,
                "maven",
                &artifact.name,
                auth.token.id,
                Some(repo.id),
            )
            .await
            .map_err(Error::db)?
        };
        if package.repo_id != Some(repo.id) {
            return Err(Error::missing());
        }
        let blob = codeyard_app::packages::store(
            &state.pool,
            &state.storage,
            package.id,
            auth.token.id,
            bytes.to_vec(),
            codeyard_db_repos::packages::Charge::Upload,
        )
        .await
        .map_err(Error::db)?;
        let (group, name) = artifact.name.split_once(':').ok_or_else(Error::invalid)?;
        let metadata = serde_json::json!({"groupId":group,"artifactId":name}).to_string();
        codeyard_db_repos::packages::put_maven_file(
            &state.pool,
            auth.token.id,
            MavenFile {
                package_id: package.id,
                repo_id: repo.id,
                path: &artifact.path,
                version: artifact.version.as_deref(),
                metadata: &metadata,
                mutable: artifact.metadata
                    || artifact
                        .version
                        .as_ref()
                        .is_some_and(|v| v.ends_with("-SNAPSHOT")),
                blob: &blob,
            },
        )
        .await
        .map_err(Error::db)?;
        Ok(StatusCode::CREATED.into_response())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn middleware_future_is_heap_bound() {
        fn size<F: std::future::Future>(
            _: impl FnOnce(
                axum::extract::State<std::sync::Arc<codeyard_app::app::AppState>>,
                axum::extract::Request,
                axum::middleware::Next,
            ) -> F,
        ) -> usize {
            std::mem::size_of::<F>()
        }
        assert_eq!(
            size(crate::packages::maven::route),
            2 * std::mem::size_of::<usize>()
        );
    }
    #[test]
    fn repository_pages_are_not_maven_artifacts() {
        assert!(crate::packages::maven::artifact("/alice/repo/tree/main/readme.md").is_none());
        assert!(crate::packages::maven::artifact("/alice/repo/raw/main/file.jar").is_none());
        let a = crate::packages::maven::artifact(
            "/alice/repo/com/example/demo/1.0-SNAPSHOT/demo-1.0-20261001.010203-1.jar.sha1",
        )
        .unwrap();
        assert_eq!(a.name, "com.example:demo");
        assert_eq!(a.version.as_deref(), Some("1.0-SNAPSHOT"));
        assert_eq!(a.checksum.as_deref(), Some("sha1"));
    }
}
