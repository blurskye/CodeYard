//! Offset cursors keep forward and backward pages in the same ordering.
use crate::arguments::Arguments;
use crate::Result;
use async_graphql::Error;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use serde_json::Value;

pub(super) struct Page {
    pub offset: i64,
    pub limit: i64,
    total: i64,
    end: i64,
}

pub(super) fn cursor(offset: i64) -> String {
    STANDARD.encode(format!("cursor:{offset}"))
}

fn decode(value: &str) -> Result<i64> {
    let decoded = STANDARD
        .decode(value)
        .map_err(|_| Error::new("Invalid cursor"))?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| Error::new("Invalid cursor"))?;
    decoded
        .strip_prefix("cursor:")
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 0 && *n < i64::MAX)
        .ok_or_else(|| Error::new("Invalid cursor"))
}

pub(super) fn validate(ctx: &Arguments) -> Result<(Option<i64>, Option<i64>)> {
    let integer = |name| {
        ctx.args
            .get(name)
            .filter(|v| !v.is_null())
            .map(|v| v.i64())
            .transpose()
    };
    let first = integer("first")?;
    let last = integer("last")?;
    if first.is_none() && last.is_none() && !ctx.count_only {
        return Err(Error::new(
            "You must provide a first or last value to properly paginate the connection.",
        ));
    }
    if first
        .into_iter()
        .chain(last)
        .any(|n| !(0..=100).contains(&n))
    {
        return Err(Error::new("first and last must be between 0 and 100"));
    }
    Ok((first, last))
}

impl Page {
    pub fn new(ctx: &Arguments, total: i64) -> Result<Self> {
        let (first, last) = validate(ctx)?;
        let bound = |name| {
            ctx.args
                .get(name)
                .filter(|v| !v.is_null())
                .map(|v| decode(v.string()?))
                .transpose()
        };
        if first.is_none() && last.is_none() {
            return Ok(Self {
                offset: 0,
                limit: 0,
                total,
                end: 0,
            });
        }
        let start = bound("after")?.map(|n| n + 1).unwrap_or(0).min(total);
        let mut end = bound("before")?.unwrap_or(total).min(total).max(start);
        if let Some(first) = first {
            end = end.min(start.saturating_add(first));
        }
        let offset = last.map(|last| start.max(end - last)).unwrap_or(start);
        Ok(Self {
            offset,
            limit: end - offset,
            total,
            end,
        })
    }

    pub fn finish(self, nodes: Vec<Value>) -> Value {
        let edges: Vec<_> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| json!({"cursor":cursor(self.offset + i as i64),"node":node}))
            .collect();
        json!({"totalCount":self.total,"nodes":nodes,"edges":edges,"pageInfo":{
            "hasPreviousPage":self.offset > 0,"hasNextPage":self.end < self.total,
            "startCursor":edges.first().map(|edge| &edge["cursor"]),"endCursor":edges.last().map(|edge| &edge["cursor"])
        }})
    }
}

pub(super) fn slice(ctx: &Arguments, nodes: Vec<Value>) -> Result<Value> {
    let page = Page::new(ctx, nodes.len() as i64)?;
    let nodes = nodes
        .into_iter()
        .skip(page.offset as usize)
        .take(page.limit as usize)
        .collect();
    Ok(page.finish(nodes))
}
