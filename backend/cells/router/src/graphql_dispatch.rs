//! Whole-document validation precedes dispatch. Mutations retain root-field order.
use crate::{
    directory::Failure,
    graphql::{Invalid, Plan, Target},
    http::Transport,
    routes::{ReadyCell, Routes},
    selection::Credential,
};
use anyhow::{ensure, Result};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[derive(Clone, Copy)]
pub struct Config {
    pub maximum_bytes: usize,
    pub maximum_cells: usize,
    pub parallel_cells: usize,
    pub read_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            maximum_bytes: 64 * 1024 * 1024,
            maximum_cells: 64,
            parallel_cells: 8,
            read_seconds: 30,
        }
    }
}
#[derive(Clone)]
pub struct Dispatcher {
    routes: Routes,
    transport: Transport,
    config: Config,
}
struct Group {
    cell: ReadyCell,
    keys: BTreeSet<String>,
    // A nodes field is sent to each owning cell; only that cell's positions survive.
    positions: BTreeMap<String, BTreeSet<usize>>,
}
impl Dispatcher {
    pub fn new(routes: Routes, transport: Transport, config: Config) -> Result<Self> {
        ensure!(
            (1024..=64 * 1024 * 1024).contains(&config.maximum_bytes)
                && (1..=256).contains(&config.maximum_cells)
                && (1..=32).contains(&config.parallel_cells)
                && (1..=300).contains(&config.read_seconds),
            "Invalid GraphQL dispatch limits"
        );
        Ok(Self {
            routes,
            transport,
            config,
        })
    }
    async fn send(
        &self,
        cell: &ReadyCell,
        credential: &Credential,
        headers: &HeaderMap,
        body: Value,
    ) -> Result<Response> {
        let mut request = Request::builder().method("POST").uri("/api/graphql");
        *request.headers_mut().ok_or(Failure::Invalid)? = headers.clone();
        request
            .headers_mut()
            .ok_or(Failure::Invalid)?
            .remove(header::CONTENT_LENGTH);
        request.headers_mut().ok_or(Failure::Invalid)?.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        self.transport
            .forward(
                cell,
                credential.value(),
                request.body(Body::from(serde_json::to_vec(&body)?))?,
            )
            .await
    }
    async fn read(&self, response: Response) -> Result<Value> {
        let bytes = tokio::time::timeout(
            Duration::from_secs(self.config.read_seconds),
            to_bytes(response.into_body(), self.config.maximum_bytes),
        )
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?;
        serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable.into())
    }
    async fn home(&self, credential: &Credential) -> Result<ReadyCell> {
        self.routes
            .cell(credential.prefix)
            .await?
            .ok_or_else(|| Failure::Unavailable.into())
    }
    async fn target(&self, target: &Target, home: &ReadyCell) -> Result<ReadyCell> {
        let cell = match target {
            Target::Name(name) => self.routes.by_name(name).await?,
            Target::Node(value) => {
                // Malformed opaque IDs must get the cell schema's ordinary field error.
                if codeyard_model::node_id::routed(value).is_none() {
                    return Ok(home.clone());
                }
                self.routes.by_node(value).await?
            }
            Target::Credential | Target::Global => return Ok(home.clone()),
            Target::Nodes(_) => anyhow::bail!(Failure::Invalid),
        };
        // A missing name is a normal nullable GraphQL lookup, evaluated by the home cell.
        Ok(cell.unwrap_or_else(|| home.clone()))
    }
    pub async fn execute(&self, headers: &HeaderMap, body: Value) -> Result<Response> {
        tokio::time::timeout(
            Duration::from_secs(self.config.read_seconds),
            self.execute_inner(headers, body),
        )
        .await
        .map_err(|_| Failure::Unavailable)?
    }
    async fn execute_inner(&self, headers: &HeaderMap, body: Value) -> Result<Response> {
        let Some(credential) = Credential::from_headers(headers, "session_id")? else {
            return Ok((StatusCode::UNAUTHORIZED, Json(json!({"message":"This endpoint requires you to be authenticated.","documentation_url":"https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql"}))).into_response());
        };
        let home = self.home(&credential).await?;
        // Authentication runs before query errors are exposed. This also uses the home
        // endpoint's integration and OAuth admission, without any application resolver here.
        let admission = self
            .send(&home, &credential, headers, json!({"query":"{viewer{id}}"}))
            .await?;
        if admission.status() != StatusCode::OK {
            return Ok(admission);
        }
        let admission = self.read(admission).await?;
        if admission
            .get("errors")
            .is_some_and(|errors| errors.as_array().is_some_and(|errors| !errors.is_empty()))
        {
            return Ok(Json(admission).into_response());
        }
        let plan = match Plan::parse(body).await {
            Ok(plan) => plan,
            Err(Invalid::Request) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"message":"Problems parsing JSON"})),
                )
                    .into_response())
            }
            Err(Invalid::Errors(errors)) => {
                return Ok(Json(json!({"errors":errors})).into_response())
            }
        };
        let mut groups = Vec::<Group>::new();
        for field in &plan.fields {
            if let Target::Nodes(ids) = &field.target {
                let mut by_cell = BTreeMap::new();
                for (index, id) in ids.iter().enumerate() {
                    let cell = self.target(&Target::Node(id.clone()), &home).await?;
                    by_cell
                        .entry(cell.id)
                        .or_insert_with(|| (cell, BTreeSet::new()))
                        .1
                        .insert(index);
                }
                if by_cell.is_empty() {
                    by_cell.insert(home.id, (home.clone(), BTreeSet::new()));
                }
                for (cell, positions) in by_cell.into_values() {
                    add_group(
                        &mut groups,
                        cell,
                        &field.key,
                        Some(positions),
                        plan.mutation,
                    );
                }
            } else {
                let cell = self.target(&field.target, &home).await?;
                add_group(&mut groups, cell, &field.key, None, plan.mutation);
            }
            ensure!(
                groups
                    .iter()
                    .map(|group| group.cell.id)
                    .collect::<BTreeSet<_>>()
                    .len()
                    <= self.config.maximum_cells,
                "Query reaches too many cells"
            );
        }
        let mut data = Map::new();
        let mut errors = Vec::new();
        let mut total = 0;
        let mut null = false;
        if plan.mutation {
            for group in groups {
                let response = self
                    .send(
                        &group.cell,
                        &credential,
                        headers,
                        plan.request(&group.keys)?,
                    )
                    .await?;
                if response.status() != StatusCode::OK {
                    return Ok(response);
                }
                let response = self.read(response).await?;
                null |= merge(
                    &mut data,
                    &mut errors,
                    &group,
                    response,
                    &mut total,
                    self.config.maximum_bytes,
                )?;
                if null {
                    break;
                }
            }
        } else {
            let mut replies = stream::iter(groups.into_iter().map(|group| {
                let plan = &plan;
                let credential = &credential;
                async move {
                    let response = self
                        .send(&group.cell, credential, headers, plan.request(&group.keys)?)
                        .await?;
                    Ok::<_, anyhow::Error>((group, response))
                }
            }))
            .buffer_unordered(self.config.parallel_cells);
            while let Some(reply) = replies.next().await {
                let (group, response) = reply?;
                if response.status() != StatusCode::OK {
                    return Ok(response);
                }
                let response = self.read(response).await?;
                null |= merge(
                    &mut data,
                    &mut errors,
                    &group,
                    response,
                    &mut total,
                    self.config.maximum_bytes,
                )?;
            }
        }
        let mut response = json!({"data":if null { Value::Null } else { Value::Object(data) }});
        if !errors.is_empty() {
            response["errors"] = Value::Array(errors);
        }
        Ok(Json(response).into_response())
    }
}
fn add_group(
    groups: &mut Vec<Group>,
    cell: ReadyCell,
    key: &str,
    positions: Option<BTreeSet<usize>>,
    mutation: bool,
) {
    let index = if mutation {
        None
    } else {
        groups.iter().position(|group| group.cell.id == cell.id)
    };
    let group = match index {
        Some(index) => &mut groups[index],
        None => {
            groups.push(Group {
                cell,
                keys: BTreeSet::new(),
                positions: BTreeMap::new(),
            });
            groups.last_mut().expect("inserted group")
        }
    };
    group.keys.insert(key.to_owned());
    if let Some(positions) = positions {
        group.positions.insert(key.to_owned(), positions);
    }
}
fn merge(
    data: &mut Map<String, Value>,
    errors: &mut Vec<Value>,
    group: &Group,
    response: Value,
    total: &mut usize,
    maximum: usize,
) -> Result<bool> {
    *total = total
        .checked_add(serde_json::to_vec(&response)?.len())
        .ok_or(Failure::Unavailable)?;
    ensure!(*total <= maximum, "GraphQL response is too large");
    if let Some(reply_errors) = response.get("errors").and_then(Value::as_array) {
        for error in reply_errors {
            let path = error.get("path").and_then(Value::as_array);
            if let Some(key) = path.and_then(|path| path.first()).and_then(Value::as_str) {
                if !group.keys.contains(key) {
                    continue;
                }
                if let Some(positions) = group.positions.get(key) {
                    if let Some(index) = path.and_then(|path| path.get(1)).and_then(Value::as_u64) {
                        if !positions.contains(&(index as usize)) {
                            continue;
                        }
                    }
                }
            }
            errors.push(error.clone());
        }
    }
    let Some(reply_data) = response.get("data") else {
        anyhow::bail!(Failure::Unavailable);
    };
    if reply_data.is_null() {
        return Ok(true);
    }
    let reply_data = reply_data.as_object().ok_or(Failure::Unavailable)?;
    for key in &group.keys {
        let value = reply_data.get(key).ok_or(Failure::Unavailable)?;
        if let Some(positions) = group.positions.get(key) {
            let values = value.as_array().ok_or(Failure::Unavailable)?;
            ensure!(values.len() <= 1000, Failure::Unavailable);
            let target = data
                .entry(key.clone())
                .or_insert_with(|| Value::Array(vec![Value::Null; values.len()]));
            let target = target.as_array_mut().ok_or(Failure::Unavailable)?;
            ensure!(target.len() == values.len(), Failure::Unavailable);
            for index in positions {
                *target.get_mut(*index).ok_or(Failure::Unavailable)? =
                    values.get(*index).ok_or(Failure::Unavailable)?.clone();
            }
        } else {
            data.insert(key.clone(), value.clone());
        }
    }
    Ok(false)
}
