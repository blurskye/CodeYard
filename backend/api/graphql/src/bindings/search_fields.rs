use super::*;
use crate::bindings::Object;

impl gql::SearchResultItemConnectionResolver for Object {
    fields!("SearchResultItemConnection";
        nodes() -> Option<Vec<Option<gql::SearchResultItem>>>;
        edges() -> Option<Vec<Option<gql::SearchResultItemEdge>>>;
        page_info() -> gql::PageInfo;
        code_count() -> i32; discussion_count() -> i32; issue_count() -> i32;
        repository_count() -> i32; user_count() -> i32; wiki_count() -> i32;
        issue_search_type() -> Option<gql::IssueSearchType>;
        lexical_fallback_reason() -> Option<Vec<gql::LexicalFallbackReason>>;
    );
}
impl gql::SearchResultItemEdgeResolver for Object {
    fields!("SearchResultItemEdge"; cursor() -> String; node() -> Option<gql::SearchResultItem>;);
}
