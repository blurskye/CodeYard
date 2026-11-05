//! Routing policy declared by the application's own route constructors.
use crate::cells::ObjectKind;
use std::{cell::RefCell, collections::BTreeMap};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rule {
    Name(String),
    Id {
        parameter: String,
        root: Option<ObjectKind>,
    },
    OpaqueId(String),
    Credential,
    Login,
    Signup,
    Device,
    OAuthCallback,
    GraphQL,
    PackagePath(String),
    Global,
    Health,
}
impl Rule {
    pub fn for_path(path: &str) -> Option<Self> {
        let path = path
            .strip_prefix("/api/v3")
            .filter(|path| path.is_empty() || path.starts_with('/'))
            .unwrap_or(path);
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        for part in &parts {
            if matches!(*part, ":owner" | ":org" | ":username" | ":repo_owner") {
                return Some(Self::Name(part[1..].to_owned()));
            }
        }
        match parts.as_slice() {
            ["avatars", ":name"] => return Some(Self::Name("name".into())),
            ["health" | "ready"] => return Some(Self::Health),
            ["enter" | "login" | "session"] => return Some(Self::Login),
            ["register" | "setup"] => return Some(Self::Signup),
            ["login", "device"] => return Some(Self::Device),
            ["login", "oauth", _, "callback"] => return Some(Self::OAuthCallback),
            ["api", "graphql"] => return Some(Self::GraphQL),
            ["v2", "*path"] => return Some(Self::PackagePath("path".into())),
            ["api", "packages", "*path"] => return Some(Self::PackagePath("path".into())),
            _ => {}
        }
        if let Some(parameter) = parts
            .iter()
            .find(|part| matches!(**part, ":gist_id" | ":file_id" | ":guid" | ":uuid"))
        {
            return Some(Self::OpaqueId(parameter[1..].to_owned()));
        }
        if parts.first() == Some(&"gists") && parts.get(1) == Some(&":id") {
            return Some(Self::OpaqueId("id".into()));
        }
        for part in &parts {
            if part.starts_with(':')
                && (*part == ":id" || part.ends_with("_id"))
                && !matches!(*part, ":client_id" | ":ghsa_id")
            {
                let root = match parts.first().copied() {
                    Some("repositories") => Some(ObjectKind::Repository),
                    Some("user") => Some(ObjectKind::User),
                    Some("organizations") => Some(ObjectKind::Organization),
                    _ => None,
                };
                return Some(Self::Id {
                    parameter: part[1..].to_owned(),
                    root,
                });
            }
        }
        match parts.first().copied().unwrap_or_default() {
            "" | "assets" | "wasm" | "help" | "robots.txt" | "ai.txt" | "metrics"
            | "advisories" | "emojis" | "codes_of_conduct" | "gitignore" | "licenses" | "meta"
            | "octocat" | "versions" | "zen" | "events" | "feeds" | "explore" | "search"
            | "repositories" | ".well-known" => Some(Self::Global),
            "settings" | "admin" | "user" | "users" | "orgs" | "organizations" | "gists"
            | "notifications" | "issues" | "pulls" | "stars" | "new" | "api" | "ci" | "login"
            | "logout" | "forgot-password" | "reset-password" | "verify-email" | "blocked"
            | "_forms" | "app" | "apps" | "applications" | "app-manifests" | "installation"
            | "rate_limit" | "markdown" | "v2" | "twirp" => Some(Self::Credential),
            // These products are declared but unsupported in the GitHub route table.
            "agents"
            | "agent-tasks"
            | "agent-automations"
            | "enterprises"
            | "assignments"
            | "classrooms"
            | "marketplace_listing"
            | "credentials" => Some(Self::Global),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    pub path: String,
    pub rule: Rule,
}
#[derive(Default)]
struct Collection {
    prefixes: Vec<String>,
    routes: BTreeMap<String, Rule>,
    missing: Vec<String>,
}
thread_local! { static COLLECTING: RefCell<Option<Collection>> = const { RefCell::new(None) }; }
struct CollectGuard;
impl Drop for CollectGuard {
    fn drop(&mut self) {
        COLLECTING.with(|slot| {
            let _ = slot.borrow_mut().take();
        });
    }
}
struct PrefixGuard(bool);
impl Drop for PrefixGuard {
    fn drop(&mut self) {
        if self.0 {
            COLLECTING.with(|slot| {
                if let Some(collection) = slot.borrow_mut().as_mut() {
                    let _ = collection.prefixes.pop();
                }
            });
        }
    }
}

/// Collect synchronously while constructing the real application router, without AppState.
pub fn collect<T>(build: impl FnOnce() -> T) -> Result<(T, Vec<Route>), String> {
    COLLECTING.with(|slot| {
        if slot.borrow().is_some() {
            return Err("Nested cell route collection".to_owned());
        }
        *slot.borrow_mut() = Some(Collection::default());
        Ok(())
    })?;
    let _guard = CollectGuard;
    let result = build();
    let collection = COLLECTING
        .with(|slot| slot.borrow_mut().take())
        .expect("owned route collection");
    if !collection.missing.is_empty() {
        return Err(format!(
            "Routes without a cell policy: {}",
            collection.missing.join(", ")
        ));
    }
    let routes = collection
        .routes
        .into_iter()
        .map(|(path, rule)| Route { path, rule })
        .collect();
    Ok((result, routes))
}

pub fn record(path: &str) {
    COLLECTING.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(collection) = slot.as_mut() else {
            return;
        };
        let prefix = collection.prefixes.concat();
        let full = if path == "/" && !prefix.is_empty() {
            prefix
        } else {
            format!("{prefix}{path}")
        };
        if full.len() > 4096 || collection.routes.len() + collection.missing.len() >= 10000 {
            collection.missing.push("route catalogue limit".into());
            return;
        }
        match Rule::for_path(&full) {
            Some(rule) => {
                collection.routes.insert(full, rule);
            }
            None => collection.missing.push(full),
        }
    });
}

pub fn under<T>(prefix: &str, build: impl FnOnce() -> T) -> T {
    let active = COLLECTING.with(|slot| {
        if let Some(collection) = slot.borrow_mut().as_mut() {
            collection.prefixes.push(prefix.to_owned());
            true
        } else {
            false
        }
    });
    let _guard = PrefixGuard(active);
    build()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_policies_fail_and_nested_paths_keep_their_owner() {
        let (_, routes) =
            collect(|| under("/api/v3", || record("/repos/:owner/:repo/issues/:number"))).unwrap();
        assert_eq!(routes[0].path, "/api/v3/repos/:owner/:repo/issues/:number");
        assert_eq!(routes[0].rule, Rule::Name("owner".into()));
        assert!(collect(|| record("/new-global-feature")).is_err());
        assert!(
            collect(|| record("/health")).is_ok(),
            "failed construction releases the collector"
        );
    }
}
