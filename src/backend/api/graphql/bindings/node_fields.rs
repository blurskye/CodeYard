use super::*;
impl gql::CommitCommentResolver for Object {
    fields!("CommitComment";
        id()->ID;database_id()->Option<i32>;body()->String;path()->Option<String>;position()->Option<i32>;
        author()->Option<gql::Actor>;author_association()->gql::CommentAuthorAssociation;
        created_at()->gql::DateTime;updated_at()->gql::DateTime;includes_created_edit()->bool;last_edited_at()->Option<gql::DateTime>;
        is_minimized()->bool;minimized_reason()->Option<String>;reaction_groups()->Option<Vec<gql::ReactionGroup>>;
        viewer_did_author()->bool;viewer_can_update()->bool;viewer_can_delete()->bool;
        repository()->gql::Repository;commit()->Option<gql::Commit>;url()->gql::URI;
    );
}
connection!(
    CommitCommentConnectionResolver,
    CommitCommentEdgeResolver,
    "CommitCommentConnection",
    "CommitCommentEdge",
    CommitComment,
    CommitCommentEdge
);
impl gql::TeamResolver for Object {
    fields!("Team";combined_slug()->String;id()->ID;database_id()->Option<i32>;name()->String;slug()->String;description()->Option<String>;privacy()->gql::TeamPrivacy;created_at()->gql::DateTime;updated_at()->gql::DateTime;organization()->gql::Organization;url()->gql::URI;);
}
connection!(
    MilestoneConnectionResolver,
    MilestoneEdgeResolver,
    "MilestoneConnection",
    "MilestoneEdge",
    Milestone,
    MilestoneEdge
);
connection!(
    ReleaseConnectionResolver,
    ReleaseEdgeResolver,
    "ReleaseConnection",
    "ReleaseEdge",
    Release,
    ReleaseEdge
);
connection!(
    ReleaseAssetConnectionResolver,
    ReleaseAssetEdgeResolver,
    "ReleaseAssetConnection",
    "ReleaseAssetEdge",
    ReleaseAsset,
    ReleaseAssetEdge
);
impl gql::BotResolver for Object {
    fields!("Bot";id()->ID;database_id()->Option<i32>;login()->String;url()->gql::URI;avatar_url(size:Option<i32>)->gql::URI;created_at()->gql::DateTime;);
}
connection!(
    PullRequestReviewConnectionResolver,
    PullRequestReviewEdgeResolver,
    "PullRequestReviewConnection",
    "PullRequestReviewEdge",
    PullRequestReview,
    PullRequestReviewEdge
);
connection!(
    ReviewRequestConnectionResolver,
    ReviewRequestEdgeResolver,
    "ReviewRequestConnection",
    "ReviewRequestEdge",
    ReviewRequest,
    ReviewRequestEdge
);
impl gql::ReviewRequestResolver for Object {
    fields!("ReviewRequest";as_code_owner()->bool;id()->ID;database_id()->Option<i32>;pull_request()->gql::PullRequest;requested_reviewer()->Option<gql::RequestedReviewer>;);
}
connection!(
    RefConnectionResolver,
    RefEdgeResolver,
    "RefConnection",
    "RefEdge",
    Ref,
    RefEdge
);
impl gql::BlobResolver for Object {
    fields!("Blob";id()->ID;oid()->gql::GitObjectID;byte_size()->i32;text()->Option<String>;is_binary()->Option<bool>;is_truncated()->bool;repository()->gql::Repository;);
}
impl gql::TreeResolver for Object {
    fields!("Tree";id()->ID;oid()->gql::GitObjectID;entries()->Option<Vec<gql::TreeEntry>>;repository()->gql::Repository;);
}
impl gql::TreeEntryResolver for Object {
    fields!("TreeEntry";name()->String;name_raw()->gql::Base64String;path()->Option<String>;path_raw()->Option<gql::Base64String>;mode()->i32;oid()->gql::GitObjectID;size()->i32;submodule()->Option<gql::Submodule>;r#type()->String;object()->Option<gql::GitObject>;);
}
impl gql::SubmoduleResolver for Object {}
impl gql::TagResolver for Object {
    fields!("Tag";id()->ID;oid()->gql::GitObjectID;name()->String;message()->Option<String>;target()->gql::GitObject;tagger()->Option<gql::GitActor>;repository()->gql::Repository;);
}
impl gql::ReactionResolver for Object {
    fields!("Reaction";id()->ID;database_id()->Option<i32>;created_at()->gql::DateTime;content()->gql::ReactionContent;user()->Option<gql::User>;reactable()->gql::Reactable;);
}
impl gql::ReleaseResolver for Object {
    fields!("Release";immutable()->bool;id()->ID;database_id()->Option<i32>;name()->Option<String>;tag_name()->String;description()->Option<String>;is_draft()->bool;is_latest()->bool;is_prerelease()->bool;created_at()->gql::DateTime;published_at()->Option<gql::DateTime>;author()->Option<gql::User>;url()->gql::URI;repository()->gql::Repository;tag()->Option<gql::Ref>;tag_commit()->Option<gql::Commit>;release_assets(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,name:Option<String>)->gql::ReleaseAssetConnection;);
}
impl gql::ReleaseAssetResolver for Object {
    fields!("ReleaseAsset";id()->ID;name()->String;content_type()->String;size()->i32;download_count()->i32;download_url()->gql::URI;created_at()->gql::DateTime;updated_at()->gql::DateTime;);
}
impl gql::PullRequestReviewResolver for Object {
    fields!("PullRequestReview";id()->ID;database_id()->Option<i32>;body()->String;reaction_groups()->Option<Vec<gql::ReactionGroup>>;state()->gql::PullRequestReviewState;created_at()->gql::DateTime;submitted_at()->Option<gql::DateTime>;author()->Option<gql::Actor>;author_association()->gql::CommentAuthorAssociation;commit()->Option<gql::Commit>;pull_request()->gql::PullRequest;url()->gql::URI;);
}
impl gql::PullRequestReviewCommentResolver for Object {
    fields!("PullRequestReviewComment";id()->ID;database_id()->Option<i32>;body()->String;reaction_groups()->Option<Vec<gql::ReactionGroup>>;path()->String;line()->Option<i32>;created_at()->gql::DateTime;updated_at()->gql::DateTime;author()->Option<gql::Actor>;pull_request()->gql::PullRequest;outdated()->bool;is_minimized()->bool;minimized_reason()->Option<String>;viewer_did_author()->bool;url()->gql::URI;);
}
macro_rules! event {($resolver:ident,$ty:literal)=>{impl gql::$resolver for Object {fields!($ty;id()->ID;created_at()->gql::DateTime;actor()->Option<gql::Actor>;);}};}
event!(MergedEventResolver, "MergedEvent");
event!(ReadyForReviewEventResolver, "ReadyForReviewEvent");
event!(ClosedEventResolver, "ClosedEvent");
event!(ReopenedEventResolver, "ReopenedEvent");
event!(LabeledEventResolver, "LabeledEvent");
event!(UnlabeledEventResolver, "UnlabeledEvent");
event!(AssignedEventResolver, "AssignedEvent");
event!(UnassignedEventResolver, "UnassignedEvent");
event!(MilestonedEventResolver, "MilestonedEvent");
event!(DemilestonedEventResolver, "DemilestonedEvent");
event!(LockedEventResolver, "LockedEvent");
event!(UnlockedEventResolver, "UnlockedEvent");
impl gql::RenamedTitleEventResolver for Object {
    fields!("RenamedTitleEvent";id()->ID;created_at()->gql::DateTime;actor()->Option<gql::Actor>;previous_title()->String;current_title()->String;);
}
