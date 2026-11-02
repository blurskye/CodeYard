//! Typed Git and status fields selected by the upstream CLI query builder.
use super::*;
impl gql::CommitResolver for Object {
    fields!("Commit";
        id() -> ID; oid() -> gql::GitObjectID; abbreviated_oid() -> String;
        message() -> String; message_headline() -> String; message_body() -> String;
        authored_date() -> gql::DateTime; committed_date() -> gql::DateTime;
        author() -> Option<gql::GitActor>; committer() -> Option<gql::GitActor>;
        authors(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::GitActorConnection;
        parents(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::CommitConnection;
        comments(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>)->gql::CommitCommentConnection;
        repository() -> gql::Repository; url() -> gql::URI;
        status_check_rollup() -> Option<gql::StatusCheckRollup>; status() -> Option<gql::Status>;
    );
}
impl gql::GitActorResolver for Object {
    fields!("GitActor"; name() -> Option<String>; email() -> Option<String>; user() -> Option<gql::User>; date() -> Option<gql::GitTimestamp>;);
}
impl gql::PullRequestCommitResolver for Object {
    fields!("PullRequestCommit"; commit() -> gql::Commit;);
}
impl gql::PullRequestChangedFileResolver for Object {
    fields!("PullRequestChangedFile"; path() -> String; additions() -> i32; deletions() -> i32; change_type() -> gql::PatchStatus;);
}
impl gql::StatusContextResolver for Object {
    fields!("StatusContext"; id() -> ID; context() -> String; state() -> gql::StatusState;
        created_at() -> gql::DateTime; description() -> Option<String>; target_url() -> Option<gql::URI>; creator() -> Option<gql::Actor>;
        is_required(pull_request_id: Option<ID>, pull_request_number: Option<i32>) -> bool;
    );
}
impl gql::StatusResolver for Object {
    fields!("Status"; id() -> ID; state() -> gql::StatusState; contexts() -> Vec<gql::StatusContext>;);
}
impl gql::StatusCheckRollupResolver for Object {
    fields!("StatusCheckRollup"; id() -> ID; state() -> gql::StatusState;
        contexts(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::StatusCheckRollupContextConnection;
    );
}
impl gql::StatusCheckRollupContextConnectionResolver for Object {
    fields!("StatusCheckRollupContextConnection";
        nodes() -> Option<Vec<Option<gql::StatusCheckRollupContext>>>; edges() -> Option<Vec<Option<gql::StatusCheckRollupContextEdge>>>;
        total_count() -> i32; page_info() -> gql::PageInfo;
        check_run_count() -> i32; status_context_count() -> i32;
        check_run_counts_by_state() -> Option<Vec<gql::CheckRunStateCount>>;
        status_context_counts_by_state() -> Option<Vec<gql::StatusContextStateCount>>;
    );
}
impl gql::StatusCheckRollupContextEdgeResolver for Object {
    fields!("StatusCheckRollupContextEdge"; node() -> Option<gql::StatusCheckRollupContext>; cursor() -> String;);
}
impl gql::StatusContextStateCountResolver for Object {
    fields!("StatusContextStateCount"; count() -> i32; state() -> gql::StatusState;);
}
impl gql::CheckRunStateCountResolver for Object {
    fields!("CheckRunStateCount"; count() -> i32; state() -> gql::CheckRunState;);
}
impl gql::BranchProtectionRuleResolver for Object {
    fields!("BranchProtectionRule";
        requires_strict_status_checks() -> bool;
        required_approving_review_count() -> Option<i32>;
    );
}
impl gql::ComparisonResolver for Object {
    fields!("Comparison"; ahead_by() -> i32; behind_by() -> i32; status() -> gql::ComparisonStatus;);
}
impl gql::AutoMergeRequestResolver for Object {
    fields!("AutoMergeRequest";
        author_email() -> Option<String>;
        commit_body() -> Option<String>;
        commit_headline() -> Option<String>;
        enabled_at() -> Option<gql::DateTime>;
        enabled_by() -> Option<gql::Actor>;
        merge_method() -> gql::PullRequestMergeMethod;
        pull_request() -> gql::PullRequest;
    );
}
connection!(
    PullRequestCommitConnectionResolver,
    PullRequestCommitEdgeResolver,
    "PullRequestCommitConnection",
    "PullRequestCommitEdge",
    PullRequestCommit,
    PullRequestCommitEdge
);
connection!(
    PullRequestChangedFileConnectionResolver,
    PullRequestChangedFileEdgeResolver,
    "PullRequestChangedFileConnection",
    "PullRequestChangedFileEdge",
    PullRequestChangedFile,
    PullRequestChangedFileEdge
);
connection!(
    GitActorConnectionResolver,
    GitActorEdgeResolver,
    "GitActorConnection",
    "GitActorEdge",
    GitActor,
    GitActorEdge
);
connection!(
    CommitConnectionResolver,
    CommitEdgeResolver,
    "CommitConnection",
    "CommitEdge",
    Commit,
    CommitEdge
);
