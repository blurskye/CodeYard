//! Bindings for issue management and relationship mutations.
use super::*;
use crate::bindings::connection;
use crate::bindings::fields;
use crate::bindings::issue_payload;
use crate::bindings::Object;
use async_graphql::ID;

impl gql::LinkedBranchResolver for Object {
    fields!("LinkedBranch"; id() -> ID; r#ref() -> Option<gql::Ref>;);
}
connection!(
    LinkedBranchConnectionResolver,
    LinkedBranchEdgeResolver,
    "LinkedBranchConnection",
    "LinkedBranchEdge",
    LinkedBranch,
    LinkedBranchEdge
);
issue_payload!(PinIssuePayloadResolver, "PinIssuePayload");
issue_payload!(UnpinIssuePayloadResolver, "UnpinIssuePayload");
issue_payload!(TransferIssuePayloadResolver, "TransferIssuePayload");
impl gql::DeleteIssuePayloadResolver for Object {
    fields!("DeleteIssuePayload"; client_mutation_id() -> Option<String>; repository() -> Option<gql::Repository>;);
}
impl gql::CreateLinkedBranchPayloadResolver for Object {
    fields!("CreateLinkedBranchPayload"; client_mutation_id() -> Option<String>; issue() -> Option<gql::Issue>; linked_branch() -> Option<gql::LinkedBranch>;);
}
connection!(
    IssueTypeConnectionResolver,
    IssueTypeEdgeResolver,
    "IssueTypeConnection",
    "IssueTypeEdge",
    IssueType,
    IssueTypeEdge
);
issue_payload!(
    UpdateIssueIssueTypePayloadResolver,
    "UpdateIssueIssueTypePayload"
);
impl gql::AddSubIssuePayloadResolver for Object {
    fields!("AddSubIssuePayload"; client_mutation_id()->Option<String>;issue()->Option<gql::Issue>;sub_issue()->Option<gql::Issue>;);
}
impl gql::RemoveSubIssuePayloadResolver for Object {
    fields!("RemoveSubIssuePayload"; client_mutation_id()->Option<String>;issue()->Option<gql::Issue>;sub_issue()->Option<gql::Issue>;);
}
impl gql::AddBlockedByPayloadResolver for Object {
    fields!("AddBlockedByPayload"; client_mutation_id()->Option<String>;issue()->Option<gql::Issue>;blocking_issue()->Option<gql::Issue>;);
}
impl gql::RemoveBlockedByPayloadResolver for Object {
    fields!("RemoveBlockedByPayload"; client_mutation_id()->Option<String>;issue()->Option<gql::Issue>;blocking_issue()->Option<gql::Issue>;);
}
