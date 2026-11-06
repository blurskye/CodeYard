//! Routing policies come from the real route constructors, including nested API aliases.
use syn::visit::Visit;

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().segments.last().is_some_and(|part| part.ident == "test")
            || matches!(&attribute.meta, syn::Meta::List(cfg) if cfg.path.is_ident("cfg") && cfg.parse_args::<syn::Path>().is_ok_and(|path| path.is_ident("test")))
    })
}

fn unchecked_route(source: &str) -> syn::Result<Option<&'static str>> {
    const METHODS: [&str; 2] = ["route", "nest"];
    #[derive(Default)]
    struct Routes {
        found: Option<&'static str>,
    }
    impl<'ast> Visit<'ast> for Routes {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            macro_rules! attributes {
                ($($kind:ident),*) => {
                    match item {
                        $(syn::Item::$kind(item) => &item.attrs,)*
                        _ => &[][..],
                    }
                };
            }
            let attributes: &[syn::Attribute] = attributes!(
                Const,
                Enum,
                ExternCrate,
                Fn,
                ForeignMod,
                Impl,
                Macro,
                Mod,
                Static,
                Struct,
                Trait,
                TraitAlias,
                Type,
                Union,
                Use
            );
            if !test_only(attributes) {
                syn::visit::visit_item(self, item);
            }
        }

        fn visit_expr_method_call(&mut self, method: &'ast syn::ExprMethodCall) {
            self.found = self
                .found
                .or_else(|| METHODS.into_iter().find(|name| method.method == *name));
            syn::visit::visit_expr_method_call(self, method);
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            let tokens: String = invocation
                .tokens
                .to_string()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            self.found = self.found.or_else(|| {
                METHODS
                    .into_iter()
                    .find(|name| tokens.contains(&format!(".{name}(")))
            });
        }
    }
    let mut routes = Routes::default();
    routes.visit_file(&syn::parse_file(source)?);
    Ok(routes.found)
}

#[test]
fn test_modules_do_not_hide_later_product_routes() {
    for declaration in [
        "#[cfg(test)] mod tests;",
        "#[cfg(test)] mod tests { fn helper() { app.route(\"/fixture\", handler); } }",
    ] {
        assert_eq!(unchecked_route(declaration).unwrap(), None);
        for method in ["route", "nest"] {
            let source =
                format!("{declaration}\nfn product() {{ app.{method}(\"/product\", handler); }}");
            assert_eq!(unchecked_route(&source).unwrap(), Some(method));
            let source = format!(
                "{declaration}\nfn product() {{ register!(app.{method}(\"/product\", handler)); }}"
            );
            assert_eq!(unchecked_route(&source).unwrap(), Some(method));
        }
    }
}

#[test]
fn every_product_route_declares_a_cell_policy() {
    let routes = codeyard_api_rest::routing_catalog().unwrap();
    for path in [
        "/api/v3/repos/:owner/:repo",
        "/api/v3/user",
        "/api/graphql",
        "/login",
        "/:owner/:repo/settings",
        "/:owner/:repo/info/refs",
        "/:owner/:repo/git-upload-pack",
        "/:owner/:repo/git-receive-pack",
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
            let unchecked = unchecked_route(&source)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(
                unchecked.is_none(),
                "{} bypasses cell declarations with .{}()",
                path.display(),
                unchecked.unwrap_or("")
            );
        }
    }
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("backend/api"));
}
