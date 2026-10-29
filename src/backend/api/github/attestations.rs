//! Repository, user and organization endpoints share bundles, subject indexes and live repository access.
use super::{auth, orgs, pagination::Cursor, repository, users, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        attestation_validation::PreparedBundle,
        attestations::{self, Attestation, Filter, Owner, Refusal},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
type Params = HashMap<String, String>;

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        Some(Refusal::Missing) => Error::missing(),
        None => Error::unavailable(error),
    }
}

fn checked_predicate(value: Option<&str>) -> Result<Option<&str>> {
    value
        .map(|value| {
            if !attestations::valid_predicate(value) {
                return Err(Error::validation(
                    "Attestation",
                    "predicate_type",
                    "invalid",
                ));
            }
            Ok(value)
        })
        .transpose()
}

fn document(row: &Attestation) -> Result<Value> {
    let bundle: Value = serde_json::from_str(&row.bundle).map_err(Error::unavailable)?;
    Ok(json!({"id":row.id,"repository_id":row.repo_id,"bundle":bundle,"initiator":row.initiator}))
}

const MAX_RESPONSE_BUNDLE_BYTES: usize = 16 * 1024 * 1024;
fn trim_bundles(
    page: &Cursor,
    rows: &mut Vec<Attestation>,
    digests: Option<&[String]>,
) -> Result<bool> {
    let (_, _, limit) = page.window()?;
    let mut bytes = 0;
    let mut count = 0;
    for row in rows.iter().take(limit) {
        let copies = if let Some(digests) = digests {
            // Stored bundles passed this validator when they were created.
            let bundle = PreparedBundle::new(
                &serde_json::from_str(&row.bundle).map_err(Error::unavailable)?,
            )
            .map_err(Error::unavailable)?;
            bundle
                .subjects()
                .iter()
                .filter(|digest| digests.contains(digest))
                .count()
        } else {
            1
        };
        let cost = (row.bundle.len() + 256) * copies;
        if bytes + cost > MAX_RESPONSE_BUNDLE_BYTES {
            break;
        }
        bytes += cost;
        count += 1;
    }
    if count == 0 && !rows.is_empty() {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Reduce the number of subject digests in this bulk request",
        ));
    }
    let more = rows.len() > count;
    rows.truncate(count);
    if page.before.is_some() {
        rows.reverse();
    }
    Ok(more)
}

async fn body(request: Request) -> Result<Value> {
    let bytes = axum::body::to_bytes(request.into_body(), attestations::MAX_BUNDLE_BYTES + 1024)
        .await
        .map_err(|_| Error::validation("Attestation", "bundle", "invalid"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::validation("Attestation", "body", "invalid"))
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
) -> Result<(Option<i64>, Option<Owner>, Option<i64>)> {
    if let Some(org) = params.get("org") {
        let actor = auth::authenticate(state, headers, true)
            .await?
            .ok_or_else(auth::required)?;
        let org = orgs::target(state, org).await?;
        Ok((Some(actor.user.id), Some(Owner::Organization(org.id)), None))
    } else if let Some(username) = params.get("user_id") {
        let actor = auth::authenticate(state, headers, true)
            .await?
            .ok_or_else(auth::required)?;
        let user = users::target(state, username).await?;
        Ok((Some(actor.user.id), Some(Owner::User(user.id)), None))
    } else {
        let (repo, actor) =
            repository(state, headers, &params["owner"], &params["repo"], false).await?;
        // These endpoints require a credential, even for a public repository.
        let actor = actor.ok_or_else(auth::required)?;
        Ok((Some(actor.user.id), None, Some(repo.id)))
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Cursor>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, org, repo) = scope(&state, &headers, &params).await?;
    let digest = params
        .get("subject_digest")
        .or_else(|| params.get("attestation_id"))
        .ok_or_else(Error::missing)?;
    if !attestations::valid_digest(digest) {
        return Err(Error::validation(
            "Attestation",
            "subject_digest",
            "invalid",
        ));
    }
    let digests = vec![digest.to_owned()];
    let (after, before, limit) = page.window()?;
    let filter = Filter {
        actor,
        owner: org,
        repo,
        digests: Some(&digests),
        predicate: checked_predicate(page.predicate_type.as_deref())?,
        after,
        before,
        limit: (limit + 1) as i64,
    };
    let mut rows = attestations::list(&state.pool, &filter)
        .await
        .map_err(Error::unavailable)?;
    let more = trim_bundles(&page, &mut rows, None)?;
    let (headers, _) = page.links(
        &uri,
        rows.first().map(|row| row.id),
        rows.last().map(|row| row.id),
        more,
    )?;
    let rows = rows.iter().map(document).collect::<Result<Vec<_>>>()?;
    Ok((headers, Json(json!({"attestations":rows}))).into_response())
}

#[derive(Deserialize)]
struct BulkList {
    subject_digests: Vec<String>,
    predicate_type: Option<String>,
}

async fn bulk_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Cursor>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Result<Response> {
    let (actor, org, repo) = scope(&state, &headers, &params).await?;
    let input: BulkList = serde_json::from_value(body(request).await?)
        .map_err(|_| Error::validation("Attestation", "subject_digests", "invalid"))?;
    if input.subject_digests.is_empty()
        || input.subject_digests.len() > 1024
        || input
            .subject_digests
            .iter()
            .any(|digest| !attestations::valid_digest(digest))
    {
        return Err(Error::validation(
            "Attestation",
            "subject_digests",
            "invalid",
        ));
    }
    let (after, before, limit) = page.window()?;
    let filter = Filter {
        actor,
        owner: org,
        repo,
        digests: Some(&input.subject_digests),
        predicate: checked_predicate(input.predicate_type.as_deref())?,
        after,
        before,
        limit: (limit + 1) as i64,
    };
    let mut rows = attestations::list(&state.pool, &filter)
        .await
        .map_err(Error::unavailable)?;
    let more = trim_bundles(&page, &mut rows, Some(&input.subject_digests))?;
    let (headers, info) = page.links(
        &uri,
        rows.first().map(|row| row.id),
        rows.last().map(|row| row.id),
        more,
    )?;
    let mut mapping: BTreeMap<String, Vec<Value>> = input
        .subject_digests
        .iter()
        .map(|digest| (digest.clone(), Vec::new()))
        .collect();
    for row in &rows {
        let value = document(row)?;
        let parsed = PreparedBundle::new(&value["bundle"]).map_err(Error::unavailable)?;
        for digest in parsed.subjects() {
            if let Some(values) = mapping.get_mut(digest) {
                values.push(value.clone());
            }
        }
    }
    Ok((
        headers,
        Json(json!({"attestations_subject_digests":mapping,"page_info":info})),
    )
        .into_response())
}

async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Cursor>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, org, repo) = scope(&state, &headers, &params).await?;
    let (after, before, limit) = page.window()?;
    let filter = Filter {
        actor,
        owner: org,
        repo,
        digests: None,
        predicate: checked_predicate(page.predicate_type.as_deref())?,
        after,
        before,
        limit: (limit + 1) as i64,
    };
    let mut rows = attestations::repositories(&state.pool, &filter)
        .await
        .map_err(Error::unavailable)?;
    let more = page.trim(&mut rows)?;
    let (headers, _) = page.links(
        &uri,
        rows.first().map(|row| row.0),
        rows.last().map(|row| row.0),
        more,
    )?;
    let rows: Vec<_> = rows
        .into_iter()
        .map(|(id, name)| json!({"id":id,"name":name}))
        .collect();
    Ok((headers, Json(rows)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    subject_digests: Option<Vec<String>>,
    attestation_ids: Option<Vec<i64>>,
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let owner = if let Some(org) = params.get("org") {
        Owner::Organization(orgs::target(&state, org).await?.id)
    } else {
        Owner::User(users::target(&state, &params["user_id"]).await?.id)
    };
    let bulk = request.method() == Method::POST;
    let (ids, digests) = if bulk {
        let input: DeleteInput = serde_json::from_value(body(request).await?)
            .map_err(|_| Error::validation("Attestation", "body", "invalid"))?;
        match (input.attestation_ids, input.subject_digests) {
            (Some(ids), None)
                if !ids.is_empty() && ids.len() <= 1024 && ids.iter().all(|id| *id > 0) =>
            {
                (ids, vec![])
            }
            (None, Some(digests))
                if !digests.is_empty()
                    && digests.len() <= 1024
                    && digests
                        .iter()
                        .all(|digest| attestations::valid_digest(digest)) =>
            {
                (vec![], digests)
            }
            _ => return Err(Error::validation("Attestation", "body", "invalid")),
        }
    } else if let Some(id) = params.get("attestation_id") {
        (
            vec![id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(Error::missing)?],
            vec![],
        )
    } else {
        let digest = &params["subject_digest"];
        if !attestations::valid_digest(digest) {
            return Err(Error::validation(
                "Attestation",
                "subject_digest",
                "invalid",
            ));
        }
        (vec![], vec![digest.clone()])
    };
    let count = attestations::delete(&state.pool, (actor.user.id, actor.token.id), owner, &ids, &digests)
        .await
        .map_err(failure)?;
    if count == 0 {
        return Err(Error::missing());
    }
    Ok(if bulk {
        StatusCode::OK
    } else {
        StatusCode::NO_CONTENT
    }
    .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-attestations", get(list)),
        ("orgs/list-attestations-bulk", post(bulk_list)),
        ("orgs/list-attestation-repositories", get(repositories)),
        ("orgs/delete-attestations-bulk", post(remove)),
        ("orgs/delete-attestations-by-subject-digest", delete(remove)),
        ("orgs/delete-attestations-by-id", delete(remove)),
        ("users/list-attestations", get(list)),
        ("users/list-attestations-bulk", post(bulk_list)),
        ("users/delete-attestations-bulk", post(remove)),
        (
            "users/delete-attestations-by-subject-digest",
            delete(remove),
        ),
        ("users/delete-attestations-by-id", delete(remove)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    fn row(id: i64, bundle: String) -> Attestation {
        Attestation {
            id,
            repo_id: 1,
            bundle,
            predicate_type: "provenance".into(),
            initiator: "user".into(),
        }
    }
    fn page() -> Cursor {
        Cursor {
            per_page: Some(100),
            before: None,
            after: None,
            predicate_type: None,
        }
    }

    #[test]
    fn bundle_bytes_limit_pages_without_skipping_associations() {
        let mut rows: Vec<_> = (1..=20)
            .map(|id| row(id, "x".repeat(attestations::MAX_BUNDLE_BYTES)))
            .collect();
        assert!(trim_bundles(&page(), &mut rows, None).unwrap());
        assert_eq!(rows.first().unwrap().id, 1);
        assert_eq!(rows.last().unwrap().id, 15);
    }

    #[test]
    fn bulk_subject_expansion_is_bounded_before_cloning_bundles() {
        let digests: Vec<_> = (0..100).map(|id| format!("sha256:{id:064x}")).collect();
        let subjects: Vec<_> = digests
            .iter()
            .map(|digest| json!({"name":"artifact","digest":{"sha256":&digest[7..]}}))
            .collect();
        let statement = json!({"_type":"https://in-toto.io/Statement/v1","subject":subjects,"predicateType":"https://slsa.dev/provenance/v1","predicate":{}});
        let value = json!({"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{"publicKey":{"hint":"fixture"},"padding":"x".repeat(900_000)},"dsseEnvelope":{"payloadType":"application/vnd.in-toto+json","payload":STANDARD.encode(statement.to_string()),"signatures":[{"sig":"eA=="}]}});
        assert_eq!(PreparedBundle::new(&value).unwrap().subjects().len(), 100);
        let mut rows = vec![row(1, value.to_string())];
        assert_eq!(
            trim_bundles(&page(), &mut rows, Some(&digests))
                .unwrap_err()
                .into_response()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
