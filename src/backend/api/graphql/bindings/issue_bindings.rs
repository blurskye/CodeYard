//! Bindings for issue management and relationship mutations.
use super::*;

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
issue_payload!(AddSubIssuePayloadResolver, "AddSubIssuePayload");
issue_payload!(RemoveSubIssuePayloadResolver, "RemoveSubIssuePayload");
issue_payload!(AddBlockedByPayloadResolver, "AddBlockedByPayload");
issue_payload!(RemoveBlockedByPayloadResolver, "RemoveBlockedByPayload");
