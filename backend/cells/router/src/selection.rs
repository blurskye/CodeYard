//! The same catalogue that builds the cell HTTP routes selects their owning cell.
use crate::{
    directory::Failure,
    routes::{ReadyCell, Routes},
};
use anyhow::{ensure, Result};
use axum::http::{header, HeaderMap};
use codeyard_model::{
    cell_routes::{Route, Rule},
    cells::{Key, KeyKind, Object, ObjectKind},
};
use codeyard_support::{
    cells::{Prefix, Secret},
    http_credentials,
    token_hash::token_hash,
    tokens,
};
use std::collections::BTreeMap;

pub struct Catalog(matchit::Router<Rule>);
pub struct Matched<'a> {
    pub rule: &'a Rule,
    parameters: BTreeMap<String, String>,
}
impl Catalog {
    pub fn new(routes: Vec<Route>) -> Result<Self> {
        ensure!(
            !routes.is_empty() && routes.len() <= 10000,
            "Invalid route catalogue"
        );
        let mut matcher = matchit::Router::new();
        for route in routes {
            ensure!(route.path.len() <= 4096, "Invalid route catalogue");
            matcher.insert(route.path, route.rule)?;
        }
        Ok(Self(matcher))
    }
    pub fn at(&self, path: &str) -> Result<Option<Matched<'_>>> {
        ensure!(
            path.len() <= 16384 && path.starts_with('/'),
            Failure::Invalid
        );
        let matched = match self.0.at(path) {
            Ok(matched) => matched,
            Err(_) => return Ok(None),
        };
        let mut parameters = BTreeMap::new();
        for (key, value) in matched.params.iter() {
            ensure!(value.len() <= 4096, Failure::Invalid);
            let value = percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .map_err(|_| Failure::Invalid)?;
            ensure!(
                !value
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control()),
                Failure::Invalid
            );
            parameters.insert(key.to_owned(), value.into_owned());
        }
        Ok(Some(Matched {
            rule: matched.value,
            parameters,
        }))
    }
}
impl Matched<'_> {
    pub fn parameter(&self, name: &str) -> Result<&str> {
        self.parameters
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| Failure::Invalid.into())
    }
}

/// An unverified routing hint. It contains the exact wire value for stable web-node
/// selection, but never authenticates it or grants access to a resource.
pub struct Credential {
    pub prefix: Prefix,
    value: String,
    token: bool,
}
impl Credential {
    pub fn value(&self) -> &str {
        &self.value
    }
    /// The cell that answers for this credential. A token follows its account after a move;
    /// a browser session stays with the cell that issued it.
    pub async fn home(&self, routes: &Routes) -> Result<Option<ReadyCell>> {
        if self.token {
            routes
                .token_home(self.prefix, &token_hash(&self.value))
                .await
        } else {
            routes.cell(self.prefix).await
        }
    }
    pub fn from_headers(headers: &HeaderMap, cookie_name: &str) -> Result<Option<Self>> {
        if headers.contains_key(header::AUTHORIZATION) {
            let value = http_credentials::token(headers)
                .map(str::to_owned)
                .or_else(|| {
                    http_credentials::basic(headers).map(|(username, password)| {
                        if password.is_empty() {
                            username
                        } else {
                            password
                        }
                    })
                })
                .ok_or(Failure::Denied)?;
            let prefix = [
                tokens::PAT_PREFIX,
                tokens::FGPAT_PREFIX,
                tokens::INSTALLATION_PREFIX,
                "ghu_",
                "gho_",
            ]
            .into_iter()
            .find_map(|namespace| {
                Secret::token(&value, namespace)
                    .ok()
                    .map(|secret| secret.cell)
            })
            .ok_or(Failure::Denied)?;
            return Ok(Some(Self {
                prefix,
                value,
                token: true,
            }));
        }
        let has_cookie = headers.get_all(header::COOKIE).iter().any(|value| {
            value.to_str().map_or(true, |value| {
                value.split(';').any(|value| {
                    value
                        .trim()
                        .split_once('=')
                        .is_some_and(|(name, _)| name == cookie_name)
                })
            })
        });
        if !has_cookie {
            return Ok(None);
        }
        let value = http_credentials::cookie(headers, cookie_name, 70).ok_or(Failure::Denied)?;
        let prefix = Secret::cookie(&value).map_err(|_| Failure::Denied)?.cell;
        Ok(Some(Self {
            prefix,
            value,
            token: false,
        }))
    }
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Routes {
    pub async fn by_key(&self, kind: KeyKind, value: &str) -> Result<Option<ReadyCell>> {
        let key = Key::new(kind, value).map_err(|_| Failure::Invalid)?;
        let Some(location) = self.location(&key).await? else {
            return Ok(None);
        };
        Ok(Some(
            self.cell(location.cell)
                .await?
                .ok_or(Failure::Unavailable)?,
        ))
    }
    pub async fn by_node(&self, value: &str) -> Result<Option<ReadyCell>> {
        let node = codeyard_model::node_id::routed(value).ok_or(Failure::Invalid)?;
        let scoped = codeyard_model::node_id::scoped_root(value).is_some();
        let kind = match node.kind {
            "I" | "PR" | "IC" | "PC" | "PRR" | "PRRC" | "D" | "DIC" | "L" | "M" | "REA" | "RE"
            | "RA" | "CS" | "CSU" | "CRU" | "IE" | "S" | "ART" | "CC" | "LB" | "EN" | "G"
            | "DE" | "DES" | "DPR" | "GBP" | "RAC" | "RI" | "IFV" | "WF" | "WR" | "J" | "CR"
            | "GIST" | "GC" | "RR" | "RRT"
                if scoped =>
            {
                Some(ObjectKind::Repository)
            }
            "T" | "OI" | "AIR" if scoped => Some(ObjectKind::Organization),
            "R" | "REF" | "C" | "B" | "TAG" | "SCR" | "STAT" | "DC" => Some(ObjectKind::Repository),
            "U" => Some(ObjectKind::User),
            "O" => Some(ObjectKind::Organization),
            _ => None,
        };
        match kind {
            Some(kind) => {
                self.by_object(Object {
                    kind,
                    id: node.root,
                })
                .await
            }
            None => {
                self.cell(Prefix::for_id(node.root).map_err(|_| Failure::Invalid)?)
                    .await
            }
        }
    }
    pub async fn for_parameter(&self, matched: &Matched<'_>) -> Result<Option<ReadyCell>> {
        match matched.rule {
            Rule::Name(name) => self.by_name(matched.parameter(name)?).await,
            Rule::Id { parameter, root } => {
                let id = matched.parameter(parameter)?;
                ensure!(
                    id.len() <= 16 && id.bytes().all(|byte| byte.is_ascii_digit()),
                    Failure::Invalid
                );
                let id = id.parse::<i64>().map_err(|_| Failure::Invalid)?;
                let prefix = Prefix::for_id(id).map_err(|_| Failure::Invalid)?;
                match root {
                    Some(kind) => self.by_object(Object { kind: *kind, id }).await,
                    None => self.cell(prefix).await,
                }
            }
            _ => anyhow::bail!(Failure::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogue_matches_static_routes_before_owner_paths_and_decodes_once() {
        let catalog = Catalog::new(vec![
            Route {
                path: "/:owner/:repo".into(),
                rule: Rule::Name("owner".into()),
            },
            Route {
                path: "/users/:username".into(),
                rule: Rule::Name("username".into()),
            },
        ])
        .unwrap();
        let matched = catalog.at("/users/a%6Cice").unwrap().unwrap();
        assert_eq!(matched.rule, &Rule::Name("username".into()));
        assert_eq!(matched.parameter("username").unwrap(), "alice");
        assert!(catalog.at("/users/%FF").is_err());
        assert!(catalog.at("/users/%00").is_err());
        assert!(catalog.at("/users/alice/repo").unwrap().is_none());
    }
    #[test]
    fn header_hints_never_silently_replace_invalid_authorization_with_a_cookie() {
        let mut headers = HeaderMap::new();
        let secret = "a".repeat(64);
        headers.insert(
            header::COOKIE,
            format!("session_id=1.{secret}").parse().unwrap(),
        );
        let hint = Credential::from_headers(&headers, "session_id")
            .unwrap()
            .unwrap();
        assert_eq!(hint.prefix, Prefix::new(1));
        assert!(!format!("{hint:?}").contains(&secret));
        headers.insert(header::AUTHORIZATION, "Bearer garbage".parse().unwrap());
        assert!(Credential::from_headers(&headers, "session_id").is_err());
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer cy_pat_2_{secret}").parse().unwrap(),
        );
        assert_eq!(
            Credential::from_headers(&headers, "session_id")
                .unwrap()
                .unwrap()
                .prefix,
            Prefix::new(2)
        );
        headers.append(
            header::AUTHORIZATION,
            format!("Bearer cy_pat_3_{secret}").parse().unwrap(),
        );
        assert!(Credential::from_headers(&headers, "session_id").is_err());
    }
}
