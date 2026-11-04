//! Typed mutation entry points; implementations delegate to the shared domain operations.
use super::*;
use crate::bindings::Object;

impl gql::MutationResolver for Object {
    projects::mutation_fields!();
    discussions::mutation_fields!();
    fields!("Mutation";
        create_issue(input: gql::Input<gql::CreateIssueInput>) -> Option<gql::CreateIssuePayload>;
        add_comment(input: gql::Input<gql::AddCommentInput>) -> Option<gql::AddCommentPayload>;
        close_issue(input: gql::Input<gql::CloseIssueInput>) -> Option<gql::CloseIssuePayload>;
        reopen_issue(input: gql::Input<gql::ReopenIssueInput>) -> Option<gql::ReopenIssuePayload>;
        update_issue(input: gql::Input<gql::UpdateIssueInput>) -> Option<gql::UpdateIssuePayload>;
        pin_issue(input: gql::Input<gql::PinIssueInput>) -> Option<gql::PinIssuePayload>;
        unpin_issue(input: gql::Input<gql::UnpinIssueInput>) -> Option<gql::UnpinIssuePayload>;
        delete_issue(input: gql::Input<gql::DeleteIssueInput>) -> Option<gql::DeleteIssuePayload>;
        transfer_issue(input: gql::Input<gql::TransferIssueInput>) -> Option<gql::TransferIssuePayload>;
        create_linked_branch(input: gql::Input<gql::CreateLinkedBranchInput>) -> Option<gql::CreateLinkedBranchPayload>;
        update_issue_issue_type(input: gql::Input<gql::UpdateIssueIssueTypeInput>) -> Option<gql::UpdateIssueIssueTypePayload>;
        add_sub_issue(input: gql::Input<gql::AddSubIssueInput>) -> Option<gql::AddSubIssuePayload>;
        remove_sub_issue(input: gql::Input<gql::RemoveSubIssueInput>) -> Option<gql::RemoveSubIssuePayload>;
        add_blocked_by(input: gql::Input<gql::AddBlockedByInput>) -> Option<gql::AddBlockedByPayload>;
        remove_blocked_by(input: gql::Input<gql::RemoveBlockedByInput>) -> Option<gql::RemoveBlockedByPayload>;
        create_pull_request(input: gql::Input<gql::CreatePullRequestInput>) -> Option<gql::CreatePullRequestPayload>;
        update_pull_request(input: gql::Input<gql::UpdatePullRequestInput>) -> Option<gql::UpdatePullRequestPayload>;
        close_pull_request(input: gql::Input<gql::ClosePullRequestInput>) -> Option<gql::ClosePullRequestPayload>;
        reopen_pull_request(input: gql::Input<gql::ReopenPullRequestInput>) -> Option<gql::ReopenPullRequestPayload>;
        convert_pull_request_to_draft(input: gql::Input<gql::ConvertPullRequestToDraftInput>) -> Option<gql::ConvertPullRequestToDraftPayload>;
        mark_pull_request_ready_for_review(input: gql::Input<gql::MarkPullRequestReadyForReviewInput>) -> Option<gql::MarkPullRequestReadyForReviewPayload>;
        merge_pull_request(input: gql::Input<gql::MergePullRequestInput>) -> Option<gql::MergePullRequestPayload>;
        enable_pull_request_auto_merge(input: gql::Input<gql::EnablePullRequestAutoMergeInput>) -> Option<gql::EnablePullRequestAutoMergePayload>;
        disable_pull_request_auto_merge(input: gql::Input<gql::DisablePullRequestAutoMergeInput>) -> Option<gql::DisablePullRequestAutoMergePayload>;
        revert_pull_request(input: gql::Input<gql::RevertPullRequestInput>) -> Option<gql::RevertPullRequestPayload>;
        add_star(input: gql::Input<gql::AddStarInput>) -> Option<gql::AddStarPayload>;
        remove_star(input: gql::Input<gql::RemoveStarInput>) -> Option<gql::RemoveStarPayload>;
        update_subscription(input: gql::Input<gql::UpdateSubscriptionInput>) -> Option<gql::UpdateSubscriptionPayload>;
        follow_user(input: gql::Input<gql::FollowUserInput>) -> Option<gql::FollowUserPayload>;
        unfollow_user(input: gql::Input<gql::UnfollowUserInput>) -> Option<gql::UnfollowUserPayload>;
        create_repository(input:gql::Input<gql::CreateRepositoryInput>)->Option<gql::CreateRepositoryPayload>;
        clone_template_repository(input:gql::Input<gql::CloneTemplateRepositoryInput>)->Option<gql::CloneTemplateRepositoryPayload>;
        update_repository(input:gql::Input<gql::UpdateRepositoryInput>)->Option<gql::UpdateRepositoryPayload>;
        archive_repository(input:gql::Input<gql::ArchiveRepositoryInput>)->Option<gql::ArchiveRepositoryPayload>;
        unarchive_repository(input:gql::Input<gql::UnarchiveRepositoryInput>)->Option<gql::UnarchiveRepositoryPayload>;
        update_pull_request_branch(input:gql::Input<gql::UpdatePullRequestBranchInput>)->Option<gql::UpdatePullRequestBranchPayload>;
        add_reaction(input:gql::Input<gql::AddReactionInput>)->Option<gql::AddReactionPayload>;
        remove_reaction(input:gql::Input<gql::RemoveReactionInput>)->Option<gql::RemoveReactionPayload>;
        update_issue_comment(input:gql::Input<gql::UpdateIssueCommentInput>)->Option<gql::UpdateIssueCommentPayload>;
        delete_issue_comment(input:gql::Input<gql::DeleteIssueCommentInput>)->Option<gql::DeleteIssueCommentPayload>;
        add_pull_request_review(input:gql::Input<gql::AddPullRequestReviewInput>)->Option<gql::AddPullRequestReviewPayload>;
        submit_pull_request_review(input:gql::Input<gql::SubmitPullRequestReviewInput>)->Option<gql::SubmitPullRequestReviewPayload>;
        update_pull_request_review(input:gql::Input<gql::UpdatePullRequestReviewInput>)->Option<gql::UpdatePullRequestReviewPayload>;
        delete_pull_request_review(input:gql::Input<gql::DeletePullRequestReviewInput>)->Option<gql::DeletePullRequestReviewPayload>;
        dismiss_pull_request_review(input:gql::Input<gql::DismissPullRequestReviewInput>)->Option<gql::DismissPullRequestReviewPayload>;
        request_reviews(input:gql::Input<gql::RequestReviewsInput>)->Option<gql::RequestReviewsPayload>;
        rerequest_reviews(input:gql::Input<gql::RerequestReviewsInput>)->Option<gql::RerequestReviewsPayload>;
        create_label(input:gql::Input<gql::CreateLabelInput>)->Option<gql::CreateLabelPayload>;
        update_label(input:gql::Input<gql::UpdateLabelInput>)->Option<gql::UpdateLabelPayload>;
        delete_label(input:gql::Input<gql::DeleteLabelInput>)->Option<gql::DeleteLabelPayload>;
        add_labels_to_labelable(input:gql::Input<gql::AddLabelsToLabelableInput>)->Option<gql::AddLabelsToLabelablePayload>;
        remove_labels_from_labelable(input:gql::Input<gql::RemoveLabelsFromLabelableInput>)->Option<gql::RemoveLabelsFromLabelablePayload>;
        add_assignees_to_assignable(input:gql::Input<gql::AddAssigneesToAssignableInput>)->Option<gql::AddAssigneesToAssignablePayload>;
        remove_assignees_from_assignable(input:gql::Input<gql::RemoveAssigneesFromAssignableInput>)->Option<gql::RemoveAssigneesFromAssignablePayload>;
        lock_lockable(input:gql::Input<gql::LockLockableInput>)->Option<gql::LockLockablePayload>;
        unlock_lockable(input:gql::Input<gql::UnlockLockableInput>)->Option<gql::UnlockLockablePayload>;
    );
}
impl gql::UpdateIssuePayloadResolver for Object {
    fields!("UpdateIssuePayload"; client_mutation_id() -> Option<String>; issue() -> Option<gql::Issue>; actor() -> Option<gql::Actor>;);
}
macro_rules! pull_payload {
    ($resolver:ident,$ty:literal) => {
        impl gql::$resolver for Object {
            fields!($ty; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>;);
        }
    };
}
pull_payload!(CreatePullRequestPayloadResolver, "CreatePullRequestPayload");
pull_payload!(
    ConvertPullRequestToDraftPayloadResolver,
    "ConvertPullRequestToDraftPayload"
);
pull_payload!(ClosePullRequestPayloadResolver, "ClosePullRequestPayload");
pull_payload!(ReopenPullRequestPayloadResolver, "ReopenPullRequestPayload");
pull_payload!(
    MarkPullRequestReadyForReviewPayloadResolver,
    "MarkPullRequestReadyForReviewPayload"
);
impl gql::UpdatePullRequestPayloadResolver for Object {
    fields!("UpdatePullRequestPayload"; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>; actor() -> Option<gql::Actor>;);
}
impl gql::RevertPullRequestPayloadResolver for Object {
    fields!("RevertPullRequestPayload"; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>; revert_pull_request() -> Option<gql::PullRequest>;);
}
impl gql::MergePullRequestPayloadResolver for Object {
    fields!("MergePullRequestPayload"; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>; actor() -> Option<gql::Actor>;);
}
impl gql::EnablePullRequestAutoMergePayloadResolver for Object {
    fields!("EnablePullRequestAutoMergePayload"; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>; actor() -> Option<gql::Actor>;);
}
impl gql::DisablePullRequestAutoMergePayloadResolver for Object {
    fields!("DisablePullRequestAutoMergePayload"; client_mutation_id() -> Option<String>; pull_request() -> Option<gql::PullRequest>; actor() -> Option<gql::Actor>;);
}
impl gql::AddStarPayloadResolver for Object {
    fields!("AddStarPayload"; client_mutation_id() -> Option<String>; starrable() -> Option<gql::Starrable>;);
}
impl gql::RemoveStarPayloadResolver for Object {
    fields!("RemoveStarPayload"; client_mutation_id() -> Option<String>; starrable() -> Option<gql::Starrable>;);
}
impl gql::UpdateSubscriptionPayloadResolver for Object {
    fields!("UpdateSubscriptionPayload"; client_mutation_id() -> Option<String>; subscribable() -> Option<gql::Subscribable>;);
}
impl gql::FollowUserPayloadResolver for Object {
    fields!("FollowUserPayload"; client_mutation_id() -> Option<String>; user() -> Option<gql::User>;);
}
impl gql::UnfollowUserPayloadResolver for Object {
    fields!("UnfollowUserPayload"; client_mutation_id() -> Option<String>; user() -> Option<gql::User>;);
}
impl gql::CreateRepositoryPayloadResolver for Object {
    fields!("CreateRepositoryPayload";client_mutation_id()->Option<String>;repository()->Option<gql::Repository>;);
}
impl gql::CloneTemplateRepositoryPayloadResolver for Object {
    fields!("CloneTemplateRepositoryPayload";client_mutation_id()->Option<String>;repository()->Option<gql::Repository>;);
}
impl gql::UpdateRepositoryPayloadResolver for Object {
    fields!("UpdateRepositoryPayload";client_mutation_id()->Option<String>;repository()->Option<gql::Repository>;);
}
impl gql::ArchiveRepositoryPayloadResolver for Object {
    fields!("ArchiveRepositoryPayload";client_mutation_id()->Option<String>;repository()->Option<gql::Repository>;);
}
impl gql::UnarchiveRepositoryPayloadResolver for Object {
    fields!("UnarchiveRepositoryPayload";client_mutation_id()->Option<String>;repository()->Option<gql::Repository>;);
}
pull_payload!(
    UpdatePullRequestBranchPayloadResolver,
    "UpdatePullRequestBranchPayload"
);
macro_rules! reaction_payload {($resolver:ident,$ty:literal)=>{impl gql::$resolver for Object {fields!($ty;client_mutation_id()->Option<String>;reaction()->Option<gql::Reaction>;reaction_groups()->Option<Vec<gql::ReactionGroup>>;subject()->Option<gql::Reactable>;);}};}
reaction_payload!(AddReactionPayloadResolver, "AddReactionPayload");
reaction_payload!(RemoveReactionPayloadResolver, "RemoveReactionPayload");
impl gql::UpdateIssueCommentPayloadResolver for Object {
    fields!("UpdateIssueCommentPayload";client_mutation_id()->Option<String>;issue_comment()->Option<gql::IssueComment>;);
}
impl gql::DeleteIssueCommentPayloadResolver for Object {
    fields!("DeleteIssueCommentPayload";client_mutation_id()->Option<String>;);
}
macro_rules! review_payload {($resolver:ident,$ty:literal)=>{impl gql::$resolver for Object {fields!($ty;client_mutation_id()->Option<String>;pull_request_review()->Option<gql::PullRequestReview>;);}};}
review_payload!(
    AddPullRequestReviewPayloadResolver,
    "AddPullRequestReviewPayload"
);
review_payload!(
    SubmitPullRequestReviewPayloadResolver,
    "SubmitPullRequestReviewPayload"
);
review_payload!(
    UpdatePullRequestReviewPayloadResolver,
    "UpdatePullRequestReviewPayload"
);
review_payload!(
    DeletePullRequestReviewPayloadResolver,
    "DeletePullRequestReviewPayload"
);
review_payload!(
    DismissPullRequestReviewPayloadResolver,
    "DismissPullRequestReviewPayload"
);
impl gql::RequestReviewsPayloadResolver for Object {
    fields!("RequestReviewsPayload";client_mutation_id()->Option<String>;actor()->Option<gql::Actor>;pull_request()->Option<gql::PullRequest>;requested_reviewers_edge()->Option<gql::UserEdge>;);
}
impl gql::RerequestReviewsPayloadResolver for Object {
    fields!("RerequestReviewsPayload";client_mutation_id()->Option<String>;actor()->Option<gql::Actor>;pull_request()->Option<gql::PullRequest>;requested_reviewers_edge()->Option<gql::UserEdge>;);
}
impl gql::CreateLabelPayloadResolver for Object {
    fields!("CreateLabelPayload";client_mutation_id()->Option<String>;label()->Option<gql::Label>;);
}
impl gql::UpdateLabelPayloadResolver for Object {
    fields!("UpdateLabelPayload";client_mutation_id()->Option<String>;label()->Option<gql::Label>;);
}
impl gql::DeleteLabelPayloadResolver for Object {
    fields!("DeleteLabelPayload";client_mutation_id()->Option<String>;);
}
impl gql::AddLabelsToLabelablePayloadResolver for Object {
    fields!("AddLabelsToLabelablePayload";client_mutation_id()->Option<String>;labelable()->Option<gql::Labelable>;);
}
impl gql::RemoveLabelsFromLabelablePayloadResolver for Object {
    fields!("RemoveLabelsFromLabelablePayload";client_mutation_id()->Option<String>;labelable()->Option<gql::Labelable>;);
}
impl gql::AddAssigneesToAssignablePayloadResolver for Object {
    fields!("AddAssigneesToAssignablePayload";client_mutation_id()->Option<String>;assignable()->Option<gql::Assignable>;);
}
impl gql::RemoveAssigneesFromAssignablePayloadResolver for Object {
    fields!("RemoveAssigneesFromAssignablePayload";client_mutation_id()->Option<String>;assignable()->Option<gql::Assignable>;);
}
impl gql::LockLockablePayloadResolver for Object {
    fields!("LockLockablePayload";client_mutation_id()->Option<String>;actor()->Option<gql::Actor>;locked_record()->Option<gql::Lockable>;);
}
impl gql::UnlockLockablePayloadResolver for Object {
    fields!("UnlockLockablePayload";client_mutation_id()->Option<String>;actor()->Option<gql::Actor>;unlocked_record()->Option<gql::Lockable>;);
}
