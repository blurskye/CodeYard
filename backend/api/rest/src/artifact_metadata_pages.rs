//! Browser inventory reads share the metadata repository authority and bounded database projections.
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_repos::artifact_metadata::Record;
use codeyard_db_repos::artifact_metadata::Refusal;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::encode_component;
use codeyard_webui::pages::artifact_metadata as view;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/orgs/:org/artifacts", get(show))
        .cell_route("/orgs/:org/artifacts/:subject_digest", get(show))
}

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) = match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => (StatusCode::FORBIDDEN, "Artifact inventory access denied."),
        Some(Refusal::Missing) => (StatusCode::NOT_FOUND, "Artifact inventory not found."),
        Some(Refusal::Invalid) => (
            StatusCode::BAD_REQUEST,
            "Check the artifact inventory filters.",
        ),
        Some(Refusal::Busy) => (StatusCode::CONFLICT, "Artifact inventory is being updated."),
        Some(Refusal::TooLarge) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Artifact inventory is too large for one page.",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Artifact inventory is temporarily unavailable.",
        ),
    };
    ctx.error(status, message)
}

fn entry(row: Record, organization: &str, repository: &str) -> anyhow::Result<view::Entry> {
    let document: Value = serde_json::from_str(&row.document)?;
    let repository_href = format!(
        "/{}/{}",
        encode_component(organization),
        encode_component(repository)
    );
    let mut fields = vec![("Artifact".into(), row.name)];
    if let Some(version) = row.version {
        fields.push(("Version".into(), version));
    }
    let labels: &[(&str, &str)] = if row.kind == "storage" {
        &[
            ("registry_url", "Registry"),
            ("repository", "Registry repository"),
            ("path", "Registry path"),
            ("artifact_url", "Artifact URL"),
            ("status", "Status"),
        ]
    } else {
        &[
            ("deployment_name", "Deployment"),
            ("logical_environment", "Logical environment"),
            ("physical_environment", "Physical environment"),
            ("cluster", "Cluster"),
            ("status", "Status"),
        ]
    };
    for (key, label) in labels {
        if let Some(value) = document
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            fields.push(((*label).to_owned(), value.to_owned()));
        }
    }
    fields.push(("Created".into(), row.created_at.to_rfc3339()));
    fields.push(("Updated".into(), row.updated_at.to_rfc3339()));
    let mut tags = Vec::new();
    let mut omitted_tags = 0;
    if let Some(values) = document.get("tags").and_then(Value::as_object) {
        for (key, value) in values.iter().take(10) {
            if let Some(value) = value.as_str() {
                tags.push((key.clone(), codeyard_webui::excerpt(value, 192)));
            }
        }
        omitted_tags = values.len().saturating_sub(tags.len());
    }
    let risks = document
        .get("runtime_risks")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let bundle = row
        .attestation_id
        .map(|id| format!("{repository_href}/attestations/{id}/bundle"));
    Ok(view::Entry {
        id: row.id,
        repository: format!("{organization}/{repository}"),
        repository_href,
        fields,
        tags,
        omitted_tags,
        risks,
        bundle,
    })
}

async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let organization = match codeyard_db_orgs::organizations::profiles::get_public_organization(
        &ctx.state.pool,
        &params["org"],
    )
    .await
    {
        Ok(Some(org)) => org,
        Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Organization not found."),
        Err(error) => return failure(&ctx, error),
    };
    let digest = params
        .get("subject_digest")
        .or_else(|| ctx.query.get("digest"))
        .cloned()
        .unwrap_or_default();
    let kind = ctx
        .query
        .get("kind")
        .cloned()
        .unwrap_or_else(|| "deployment".into());
    if !matches!(kind.as_str(), "deployment" | "storage")
        || (!digest.is_empty()
            && (!digest.starts_with("sha256:")
                || !codeyard_db_core::queries::attestations::valid_digest(&digest)))
    {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "Enter a complete lowercase SHA-256 digest and choose deployment or storage records.",
        );
    }
    let after = match ctx.query.get("after") {
        Some(value) => match value
            .parse::<i64>()
            .ok()
            .filter(|id| *id >= 0 && *id < i64::MAX)
        {
            Some(id) => id,
            None => return ctx.error(StatusCode::BAD_REQUEST, "Invalid inventory page."),
        },
        None => 0,
    };
    let base = format!("/orgs/{}/artifacts", encode_component(&organization.name));
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers);
    let mut rows = if digest.is_empty() {
        vec![]
    } else {
        match codeyard_db_repos::artifact_metadata::pages::list(
            &ctx.state.pool,
            session.as_deref(),
            organization.id,
            &digest,
            &kind,
            after,
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => return failure(&ctx, error),
        }
    };
    let more = rows.len() > 30;
    rows.truncate(30);
    let next = if more {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("digest", &digest)
            .append_pair("kind", &kind)
            .append_pair(
                "after",
                &rows
                    .last()
                    .map(|row| row.record.id)
                    .unwrap_or_default()
                    .to_string(),
            )
            .finish();
        Some(format!("{base}?{query}"))
    } else {
        None
    };
    let mut entries = Vec::new();
    for row in rows {
        match entry(row.record, &organization.name, &row.repository) {
            Ok(row) => entries.push(row),
            Err(error) => return failure(&ctx, error),
        }
    }
    ctx.page_model(
        "Artifact inventory",
        AppPage::ArtifactMetadata(view::Page {
            organization_href: codeyard_webui::profile_path(&organization.name),
            organization: organization.name,
            base,
            digest,
            kind,
            entries,
            next,
        }),
    )
}
