//! The derive schema is compiled in its own crate and instantiated once.
use crate::bindings::Object;
use crate::RequestContext;
use async_graphql::extensions::Extension;
use async_graphql::extensions::ExtensionContext;
use async_graphql::extensions::ExtensionFactory;
use async_graphql::extensions::NextResolve;
use async_graphql::extensions::ResolveInfo;
use async_graphql::ServerError;
use async_graphql::ServerResult;
use async_graphql::Value;
use codeyard_github_graphql::GitHubSchema;
use codeyard_github_graphql::Mutation;
use codeyard_github_graphql::Query;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;

pub(super) fn schema() -> &'static GitHubSchema {
    static SCHEMA: OnceLock<GitHubSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        GitHubSchema::with_extensions(
            Query::new(Object(serde_json::Value::Null)),
            Mutation::new(Object(serde_json::Value::Null)),
            vec![Arc::new(Limits)],
        )
    })
}

struct Limits;

impl ExtensionFactory for Limits {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(Self)
    }
}

#[async_graphql::async_trait::async_trait]
impl Extension for Limits {
    async fn resolve(
        &self,
        ctx: &ExtensionContext<'_>,
        info: ResolveInfo<'_>,
        next: NextResolve<'_>,
    ) -> ServerResult<Option<Value>> {
        if !info.is_for_introspection && !info.name.starts_with("__") {
            if let Some(request) = ctx.data_opt::<RequestContext>() {
                if request.nodes.fetch_add(1, Ordering::Relaxed) >= 10_000 {
                    return Err(ServerError::new("Query node limit exceeded", None));
                }
            }
        }
        let unsupported = format!(
            "Codeyard does not support {}.{} yet",
            info.parent_type, info.name
        );
        let value = next.run(ctx, info).await.map_err(|mut error| {
            if matches!(error.extensions.as_ref().and_then(|e| e.get("code")), Some(Value::String(code)) if code == "NOT_IMPLEMENTED") {
                error.message = unsupported;
            }
            error
        })?;
        // Each alias of a text field is another copy in the response.
        if let (Some(Value::String(text)), Some(request)) =
            (&value, ctx.data_opt::<RequestContext>())
        {
            if request.charge(text.len()).is_err() {
                return Err(ServerError::new("Query response size limit exceeded", None));
            }
        }
        Ok(value)
    }
}
