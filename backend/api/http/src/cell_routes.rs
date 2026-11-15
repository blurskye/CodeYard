//! Record policies from the same constructors that install the application's routes.
use axum::{routing::MethodRouter, Router};

pub trait CellRoutes<S> {
    fn cell_route(self, path: &str, route: MethodRouter<S>) -> Self;
    fn cell_nest(self, path: &str, build: impl FnOnce() -> Router<S>) -> Self;
}

/// The routing policy and the route tables name parameters `:name` and `*name`; axum 0.8 reads
/// `{name}` and `{*name}`. Every route in the application is installed through the two methods
/// below, so this is the one place that converts.
pub fn axum_path(path: &str) -> String {
    path.split('/')
        .map(|segment| match segment.as_bytes() {
            [b':', rest @ ..] if !rest.is_empty() => format!("{{{}}}", &segment[1..]),
            [b'*', rest @ ..] if !rest.is_empty() => format!("{{*{}}}", &segment[1..]),
            _ => segment.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl<S: Clone + Send + Sync + 'static> CellRoutes<S> for Router<S> {
    fn cell_route(self, path: &str, route: MethodRouter<S>) -> Self {
        codeyard_types::cell_routes::record(path);
        self.route(&axum_path(path), route)
    }
    fn cell_nest(self, path: &str, build: impl FnOnce() -> Router<S>) -> Self {
        let router = codeyard_types::cell_routes::under(path, build);
        // axum 0.8 refuses to nest at the root.
        if path == "/" {
            self.merge(router)
        } else {
            self.nest(&axum_path(path), router)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::axum_path;

    #[test]
    fn policy_paths_become_axum_paths() {
        assert_eq!(
            axum_path("/repos/:owner/:repo/git/ref/*ref"),
            "/repos/{owner}/{repo}/git/ref/{*ref}"
        );
        assert_eq!(axum_path("/v2/*path"), "/v2/{*path}");
        assert_eq!(axum_path("/"), "/");
        assert_eq!(axum_path("/users/:user_id"), "/users/{user_id}");
        assert_eq!(axum_path("/plain/segments"), "/plain/segments");
    }
}
