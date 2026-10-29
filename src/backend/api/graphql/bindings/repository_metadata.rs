//! Fields exported by gh's repository JSON selection.
use super::*;

impl gql::LanguageResolver for Object {
    fields!("Language"; name() -> String; color() -> Option<String>;);
}
impl gql::LanguageConnectionResolver for Object {
    fields!("LanguageConnection";
        edges() -> Option<Vec<Option<gql::LanguageEdge>>>;
        nodes() -> Option<Vec<Option<gql::Language>>>;
        page_info() -> gql::PageInfo; total_count() -> i32; total_size() -> i32;
    );
}
impl gql::LanguageEdgeResolver for Object {
    fields!("LanguageEdge"; size() -> i32; node() -> gql::Language; cursor() -> String;);
}
impl gql::CodeOfConductResolver for Object {
    fields!("CodeOfConduct"; key() -> String; name() -> String; url() -> Option<gql::URI>;);
}
impl gql::LicenseResolver for Object {
    fields!("License"; key() -> String; name() -> String; nickname() -> Option<String>;);
}
impl gql::RepositoryContactLinkResolver for Object {
    fields!("RepositoryContactLink"; name() -> String; about() -> String; url() -> gql::URI;);
}
impl gql::FundingLinkResolver for Object {
    fields!("FundingLink"; platform() -> gql::FundingPlatform; url() -> gql::URI;);
}
impl gql::IssueTemplateResolver for Object {
    fields!("IssueTemplate"; name() -> String; title() -> Option<String>; body() -> Option<String>; about() -> Option<String>;);
}
impl gql::PullRequestTemplateResolver for Object {
    fields!("PullRequestTemplate"; body() -> Option<String>; filename() -> Option<String>;);
}
