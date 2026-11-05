//! Run the shared schema's full validation before any cross-cell mutation is dispatched.
use async_graphql::{
    extensions::{Extension, ExtensionContext, ExtensionFactory, NextExecute},
    Request, Response, ServerError,
};
use codeyard_github_graphql::{GitHubSchema, Mutation, MutationResolver, Query, QueryResolver};
use std::sync::{Arc, OnceLock};

struct Unused;
impl QueryResolver for Unused {}
impl MutationResolver for Unused {}
struct Validate;
impl ExtensionFactory for Validate {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(Self)
    }
}
#[async_graphql::async_trait::async_trait]
impl Extension for Validate {
    // async-graphql calls execute only after parsing, coercion and validation.
    // Stop here: no application resolver or side effect can run in this schema.
    async fn execute(
        &self,
        _ctx: &ExtensionContext<'_>,
        _operation: Option<&str>,
        _next: NextExecute<'_>,
    ) -> Response {
        Response::default()
    }
}

pub async fn validate_request(request: Request) -> Vec<ServerError> {
    static SCHEMA: OnceLock<GitHubSchema> = OnceLock::new();
    let schema = SCHEMA.get_or_init(|| {
        GitHubSchema::with_extensions(
            Query::new(Unused),
            Mutation::new(Unused),
            vec![Arc::new(Validate)],
        )
    });
    schema.execute(request).await.errors
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn validation_checks_the_whole_document_without_running_a_resolver() {
        assert!(validate_request(Request::new("query{viewer{login}}"))
            .await
            .is_empty());
        assert!(
            validate_request(Request::new(
                "mutation{createIssue(input:{repositoryId:\"missing\",title:\"x\"}){issue{id}}}"
            ))
            .await
            .is_empty(),
            "valid mutation never reaches an unsupported resolver"
        );
        assert!(!validate_request(Request::new("mutation{createIssue(input:{repositoryId:\"missing\",title:\"x\"}){issue{id}} invalidMutation}" )).await.is_empty());
    }
}
