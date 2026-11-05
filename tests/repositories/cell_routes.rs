//! Routing policies come from the real route constructors, including nested API aliases.
#[test]
fn every_product_route_declares_a_cell_policy() {
    let routes = codeyard_api_rest::routing_catalog().unwrap();
    for path in [
        "/api/v3/repos/:owner/:repo",
        "/api/v3/user",
        "/api/graphql",
        "/login",
        "/:owner/:repo/settings",
    ] {
        assert!(
            routes.iter().any(|route| route.path == path),
            "missing route: {path}"
        );
    }
    let catalog = codeyard_cells_router::selection::Catalog::new(routes).unwrap();
    let matched = catalog.at("/api/v3/repos/alice/repo").unwrap().unwrap();
    assert_eq!(matched.parameter("owner").unwrap(), "alice");
    fn walk(path: &std::path::Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs")
                || path.file_name().is_some_and(|name| name == "tests.rs")
            {
                continue;
            }
            if path.ends_with("backend/api/http/src/cell_routes.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(
                !production.contains(".route("),
                "{} bypasses the cell route declaration",
                path.display()
            );
            assert!(
                !production.contains(".nest("),
                "{} bypasses the cell prefix declaration",
                path.display()
            );
        }
    }
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("backend/api"));
}
