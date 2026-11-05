//! Record policies from the same constructors that install the application's routes.
use axum::{routing::MethodRouter, Router};

pub trait CellRoutes<S> {
    fn cell_route(self, path: &str, route: MethodRouter<S>) -> Self;
    fn cell_nest(self, path: &str, build: impl FnOnce() -> Router<S>) -> Self;
}
impl<S: Clone + Send + Sync + 'static> CellRoutes<S> for Router<S> {
    fn cell_route(self, path: &str, route: MethodRouter<S>) -> Self {
        codeyard_model::cell_routes::record(path);
        self.route(path, route)
    }
    fn cell_nest(self, path: &str, build: impl FnOnce() -> Router<S>) -> Self {
        let router = codeyard_model::cell_routes::under(path, build);
        self.nest(path, router)
    }
}
