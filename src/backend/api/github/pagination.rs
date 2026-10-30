use super::{Error, Result};
use axum::{
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};

pub(super) fn page_size(value: Option<usize>) -> usize {
    value.unwrap_or(30).clamp(1, 100)
}

/// Opaque, exclusive ID cursors for GitHub's before/after collections.
#[derive(Default, Deserialize)]
pub(super) struct Cursor {
    pub per_page: Option<usize>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub predicate_type: Option<String>,
}

impl Cursor {
    pub fn window(&self) -> Result<(i64, i64, usize)> {
        let decode = |value: &str| -> Option<i64> {
            let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            text.strip_prefix("cursor:v1:")?
                .parse()
                .ok()
                .filter(|id| *id > 0 && *id < i64::MAX)
        };
        if self.before.is_some() && self.after.is_some() {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "Use either before or after",
            ));
        }
        let bound = |value: &Option<String>, default| {
            value
                .as_deref()
                .map(decode)
                .unwrap_or(Some(default))
                .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid cursor"))
        };
        Ok((
            bound(&self.after, 0)?,
            bound(&self.before, i64::MAX)?,
            page_size(self.per_page),
        ))
    }

    pub fn trim<T>(&self, rows: &mut Vec<T>) -> Result<bool> {
        let (_, _, limit) = self.window()?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        if self.before.is_some() {
            rows.reverse();
        }
        Ok(more)
    }

    pub fn links(
        &self,
        uri: &Uri,
        first: Option<i64>,
        last: Option<i64>,
        more: bool,
    ) -> Result<(HeaderMap, Value)> {
        let (after, before, limit) = self.window()?;
        let next = last.filter(|_| if before < i64::MAX { true } else { more });
        let previous = first.filter(|_| if before < i64::MAX { more } else { after > 0 });
        let encode = |id| URL_SAFE_NO_PAD.encode(format!("cursor:v1:{id}"));
        let mut links = Vec::new();
        for (id, key, relation) in [(next, "after", "next"), (previous, "before", "prev")] {
            if let Some(id) = id {
                let mut query: Vec<(String, String)> =
                    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                        .filter(|(key, _)| !matches!(key.as_ref(), "before" | "after" | "per_page"))
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect();
                query.push((key.to_owned(), encode(id)));
                query.push(("per_page".to_owned(), limit.to_string()));
                let query = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(query)
                    .finish();
                links.push(format!(
                    "<{}{}?{query}>; rel=\"{relation}\"",
                    crate::backend::api::http::config::link_origin(),
                    uri.path()
                ));
            }
        }
        let mut headers = HeaderMap::new();
        if !links.is_empty() {
            headers.insert(
                header::LINK,
                links.join(", ").parse().map_err(Error::unavailable)?,
            );
        }
        let mut info = json!({"has_next":next.is_some(),"has_previous":previous.is_some()});
        if let Some(id) = next {
            info["next"] = json!(encode(id));
        }
        if let Some(id) = previous {
            info["previous"] = json!(encode(id));
        }
        Ok((headers, info))
    }
}

/// The cursor after a keyset batch that asked for `size` rows, or `None` when the batch was the
/// last one.
pub(super) fn next_after<T>(rows: &[T], size: i64, key: impl Fn(&T) -> i64) -> Option<i64> {
    if rows.len() as i64 == size {
        rows.last().map(key)
    } else {
        None
    }
}

/// Cursor windows for collections whose native order is selected by the caller. Cursors carry
/// positions in that order, not database IDs, so updated/name sorting also paginates correctly.
#[derive(Deserialize)]
pub(super) struct CursorPagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub before: Option<String>,
    pub after: Option<String>,
}
impl CursorPagination {
    fn numbered(&self) -> Pagination { Pagination { page: self.page, per_page: self.per_page } }
    fn cursor(&self) -> Result<Option<(usize, bool)>> {
        if self.before.is_some() && self.after.is_some() || self.page.is_some() && (self.before.is_some() || self.after.is_some()) {
            return Err(Error::new(StatusCode::BAD_REQUEST, "Use one pagination direction"));
        }
        let Some(value) = self.before.as_deref().or(self.after.as_deref()) else { return Ok(None); };
        if value.is_empty() { return Ok(Some((0, false))); }
        let position = URL_SAFE_NO_PAD.decode(value).ok().and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|value| value.strip_prefix("offset:v1:")?.parse::<usize>().ok())
            .filter(|position| *position <= 1_000_000)
            .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid cursor"))?;
        Ok(Some((position, self.before.is_some())))
    }
    pub fn window(&self) -> Result<(usize, usize)> {
        let Some((position, before)) = self.cursor()? else { return self.numbered().window(); };
        let limit = self.per_page.unwrap_or(30).clamp(1, 100);
        Ok(if before { (position.saturating_sub(limit), limit.min(position)) } else { (position, limit) })
    }
    pub fn response(&self, uri: &Uri, mut items: Vec<Value>, more: bool) -> Result<Response> {
        let Some((_, before)) = self.cursor()? else { return self.numbered().response(uri, items, more); };
        let (offset, limit) = self.window()?;
        items.truncate(limit);
        let mut links = Vec::new();
        for (position, key, relation, include) in [(offset + items.len(), "after", "next", !items.is_empty() && (before || more)), (offset, "before", "prev", offset > 0)] {
            if !include { continue; }
            let mut query: Vec<(String, String)> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                .filter(|(key, _)| !matches!(key.as_ref(), "page" | "before" | "after" | "per_page"))
                .map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
            query.push((key.into(), URL_SAFE_NO_PAD.encode(format!("offset:v1:{position}"))));
            query.push(("per_page".into(), self.per_page.unwrap_or(30).clamp(1, 100).to_string()));
            let query = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(query).finish();
            links.push(format!("<{}{}?{query}>; rel=\"{relation}\"", crate::backend::api::http::config::link_origin(), uri.path()));
        }
        let mut headers = HeaderMap::new();
        if !links.is_empty() { headers.insert(header::LINK, links.join(", ").parse().map_err(Error::unavailable)?); }
        Ok((headers, Json(items)).into_response())
    }
}

#[derive(Deserialize)]
pub(super) struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
}

impl Pagination {
    pub fn window(&self) -> Result<(usize, usize)> {
        let page = self.page.unwrap_or(1);
        let limit = page_size(self.per_page);
        let offset = page
            .checked_sub(1)
            .and_then(|page| page.checked_mul(limit))
            .filter(|offset| *offset <= 1_000_000)
            .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid page"))?;
        Ok((offset, limit))
    }

    /// Walks a native list that is read in keyset batches to this numbered page. `batch(after)`
    /// returns the rows that count after the cursor `after`, and the cursor of the next batch
    /// when another may follow. Answers the page's rows and whether more follow them.
    pub async fn collect<T, F, Fut>(&self, mut batch: F) -> Result<(Vec<T>, bool)>
    where
        F: FnMut(i64) -> Fut,
        Fut: std::future::Future<Output = Result<(Vec<T>, Option<i64>)>>,
    {
        let (offset, limit) = self.window()?;
        let (mut after, mut skipped, mut items) = (0, 0, Vec::new());
        loop {
            let (rows, next) = batch(after).await?;
            for row in rows {
                if skipped < offset {
                    skipped += 1;
                } else if items.len() == limit {
                    return Ok((items, true));
                } else {
                    items.push(row);
                }
            }
            match next {
                Some(next) if next > after => after = next,
                _ => return Ok((items, false)),
            }
        }
    }

    pub fn response(&self, uri: &Uri, mut items: Vec<Value>, has_next: bool) -> Result<Response> {
        let (_, limit) = self.window()?;
        items.truncate(limit);
        Ok((self.headers(uri, has_next)?, Json(items)).into_response())
    }

    pub fn headers(&self, uri: &Uri, has_next: bool) -> Result<HeaderMap> {
        let (_, limit) = self.window()?;
        let page = self.page.unwrap_or(1);
        let link = |page: usize, relation: &str| {
            let mut query: Vec<(String, String)> =
                url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                    .filter(|(key, _)| key != "page" && key != "per_page")
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
            query.push(("page".into(), page.to_string()));
            query.push(("per_page".into(), limit.to_string()));
            let query = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish();
            format!(
                "<{}{}?{query}>; rel=\"{relation}\"",
                crate::backend::api::http::config::link_origin(),
                uri.path()
            )
        };
        let mut links = Vec::new();
        if has_next {
            links.push(link(page + 1, "next"));
        }
        if page > 1 {
            links.push(link(1, "first"));
            links.push(link(page - 1, "prev"));
        }
        let mut headers = HeaderMap::new();
        if !links.is_empty() {
            headers.insert(
                header::LINK,
                links.join(", ").parse().map_err(Error::unavailable)?,
            );
        }
        Ok(headers)
    }
}
