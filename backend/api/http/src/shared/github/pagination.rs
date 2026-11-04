use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
pub fn page_size(value: Option<usize>) -> usize {
    value.unwrap_or(30).clamp(1, 100)
}

#[derive(Deserialize)]
pub struct Pagination {
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
                codeyard_model::http_config::link_origin(),
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
