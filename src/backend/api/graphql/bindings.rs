//! Typed schema bindings over the authorized presentation values loaded by each family.
use super::{
    arg, collaboration, db, issue, missing, node, repository, unavailable, user, Arguments,
    RequestContext, Result, Value,
};
use async_graphql::futures_util::future::BoxFuture;
use async_graphql::{Context, Error, InputType, ID};
use codeyard_github_graphql as gql;
use serde_json::json;
use std::{future::Future, sync::atomic::Ordering};

pub(super) struct Object(pub Value);

/// Builds a family resolver's future in its own stack frame and boxes it. Without
/// this, a debug build's `Object::resolve` poll reserves stack for every arm's future
/// at once, and nested `gh` queries overflow a Tokio worker's 2 MiB stack.
pub(super) fn boxed<'a, F, Fut>(make: F) -> BoxFuture<'a, Result<Value>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Value>> + Send + 'a,
{
    Box::pin(make())
}

trait Decode: Sized {
    fn decode(value: Value) -> Result<Self>;
}

impl<T: Decode> Decode for Option<T> {
    fn decode(value: Value) -> Result<Self> {
        if value.is_null() {
            Ok(None)
        } else {
            T::decode(value).map(Some)
        }
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(value: Value) -> Result<Self> {
        match value {
            Value::Array(items) => items.into_iter().map(T::decode).collect(),
            _ => Err(unavailable(())),
        }
    }
}

macro_rules! primitives {
    ($($ty:ty),* $(,)?) => {$(
        impl Decode for $ty {
            fn decode(value: Value) -> Result<Self> {
                serde_json::from_value(value).map_err(unavailable)
            }
        }
    )*};
}
primitives!(String, bool, i32, f64);

impl Decode for ID {
    fn decode(value: Value) -> Result<Self> {
        String::decode(value).map(ID)
    }
}

macro_rules! scalars {
    ($($ty:ident),* $(,)?) => {$(
        impl Decode for gql::$ty {
            fn decode(value: Value) -> Result<Self> {
                async_graphql::Value::from_json(value).map(gql::$ty).map_err(unavailable)
            }
        }
    )*};
}
scalars!(
    DateTime,
    URI,
    GitSSHRemote,
    GitObjectID,
    GitTimestamp,
    BigInt,
    Base64String
);

macro_rules! enums {
    ($($ty:ident),* $(,)?) => {$(
        impl Decode for gql::$ty {
            fn decode(value: Value) -> Result<Self> {
                <Self as InputType>::parse(Some(async_graphql::Value::from_json(value).map_err(unavailable)?)).map_err(unavailable)
            }
        }
    )*};
}
enums!(
    PatchStatus,
    ComparisonStatus,
    MergeableState,
    MergeStateStatus,
    PullRequestReviewDecision,
    StatusState,
    CheckRunState,
    IssueSearchType,
    LexicalFallbackReason,
    LockReason,
    PullRequestReviewState,
    PullRequestReviewCommentState
);
enums!(
    IssueState,
    PullRequestState,
    RepositoryVisibility,
    RepositoryPermission,
    SubscriptionState,
    PullRequestMergeMethod,
    CommentAuthorAssociation,
    TeamPrivacy,
    IssueStateReason,
    MilestoneState,
    ReactionContent,
    FundingPlatform,
    IssueTypeColor,
    RuleEnforcement,
    RepositoryRulesetTarget,
    RepositoryRuleType
);

macro_rules! objects {
    ($($ty:ident),* $(,)?) => {$(
        impl Decode for gql::$ty {
            fn decode(value: Value) -> Result<Self> {
                if !value.is_object() { return Err(unavailable(())); }
                Ok(Self::new(Object(value)))
            }
        }
    )*};
}
objects!(
    User,
    Organization,
    Repository,
    Issue,
    PullRequest,
    Label,
    IssueComment,
    Ref,
    BranchProtectionRule,
    Comparison,
    Commit,
    RateLimit,
    PageInfo,
    LabelConnection,
    LabelEdge,
    IssueConnection,
    IssueEdge,
    PullRequestConnection,
    PullRequestEdge,
    IssueCommentConnection,
    IssueCommentEdge,
    CreateIssuePayload,
    AddCommentPayload,
    CloseIssuePayload,
    ReopenIssuePayload,
    RepositoryConnection,
    RepositoryEdge,
    UserConnection,
    UserEdge,
    RepositoryTopicConnection,
    RepositoryTopicEdge,
    RepositoryTopic,
    Topic,
    LanguageConnection,
    LanguageEdge,
    Language,
    CodeOfConduct,
    License,
    RepositoryContactLink,
    FundingLink,
    UpdateIssuePayload,
    CreatePullRequestPayload,
    UpdatePullRequestPayload,
    ClosePullRequestPayload,
    ReopenPullRequestPayload,
    MarkPullRequestReadyForReviewPayload,
    ConvertPullRequestToDraftPayload,
    RevertPullRequestPayload,
    MergePullRequestPayload,
    EnablePullRequestAutoMergePayload,
    DisablePullRequestAutoMergePayload,
    AddStarPayload,
    RemoveStarPayload,
    UpdateSubscriptionPayload,
    FollowUserPayload,
    UnfollowUserPayload,
    CreateRepositoryPayload,
    CloneTemplateRepositoryPayload,
    UpdateRepositoryPayload,
    ArchiveRepositoryPayload,
    UnarchiveRepositoryPayload,
    UpdatePullRequestBranchPayload,
    AddReactionPayload,
    RemoveReactionPayload,
    UpdateIssueCommentPayload,
    DeleteIssueCommentPayload,
    AddPullRequestReviewPayload,
    SubmitPullRequestReviewPayload,
    UpdatePullRequestReviewPayload,
    DeletePullRequestReviewPayload,
    DismissPullRequestReviewPayload,
    RequestReviewsPayload,
    RerequestReviewsPayload,
    ReviewRequest,
    ReviewRequestConnection,
    ReviewRequestEdge,
    PullRequestReviewConnection,
    PullRequestReviewEdge,
    SubIssuesSummary,
    MilestoneConnection,
    MilestoneEdge,
    ReleaseConnection,
    ReleaseEdge,
    ReleaseAssetConnection,
    ReleaseAssetEdge,
    Bot,
    Team,
    CommitComment,
    CommitCommentConnection,
    CommitCommentEdge,
    RefConnection,
    RefEdge,
    Blob,
    Tree,
    TreeEntry,
    Submodule,
    Tag,
    Reaction,
    Release,
    ReleaseAsset,
    PullRequestReview,
    PullRequestReviewComment,
    MergedEvent,
    ReadyForReviewEvent,
    ClosedEvent,
    ReopenedEvent,
    RenamedTitleEvent,
    LabeledEvent,
    UnlabeledEvent,
    AssignedEvent,
    UnassignedEvent,
    MilestonedEvent,
    DemilestonedEvent,
    LockedEvent,
    UnlockedEvent,
    CreateLabelPayload,
    UpdateLabelPayload,
    DeleteLabelPayload,
    AddLabelsToLabelablePayload,
    RemoveLabelsFromLabelablePayload,
    AddAssigneesToAssignablePayload,
    RemoveAssigneesFromAssignablePayload,
    LockLockablePayload,
    UnlockLockablePayload,
    SearchResultItemConnection,
    SearchResultItemEdge,
    GitActor,
    GitActorConnection,
    GitActorEdge,
    CommitConnection,
    CommitEdge,
    PullRequestCommit,
    PullRequestCommitConnection,
    PullRequestCommitEdge,
    PullRequestChangedFile,
    PullRequestChangedFileConnection,
    PullRequestChangedFileEdge,
    Status,
    StatusContext,
    StatusCheckRollup,
    StatusCheckRollupContextConnection,
    StatusCheckRollupContextEdge,
    CheckRunStateCount,
    StatusContextStateCount,
    AutoMergeRequest,
    Milestone,
    ReactionGroup,
    ReactingUserConnection,
    IssueType,
    ProjectCardConnection,
    ProjectCardEdge,
    ProjectCard,
    AssigneeConnection,
    AssigneeEdge,
    IssueTemplate,
    PullRequestTemplate,
    LinkedBranch,
    LinkedBranchConnection,
    LinkedBranchEdge,
    PinIssuePayload,
    UnpinIssuePayload,
    DeleteIssuePayload,
    TransferIssuePayload,
    CreateLinkedBranchPayload,
    UpdateIssueIssueTypePayload,
    AddSubIssuePayload,
    RemoveSubIssuePayload,
    AddBlockedByPayload,
    RemoveBlockedByPayload,
    IssueTypeConnection,
    IssueTypeEdge,
    Gist,
    GistConnection,
    GistEdge,
    GistFile
);

macro_rules! abstract_types {
    ($ty:ident: $($member:ident),* $(,)?) => {
        impl Decode for gql::$ty {
            fn decode(value: Value) -> Result<Self> {
                match value.get("__typename").and_then(Value::as_str) {
                    $(Some(stringify!($member)) => gql::$member::decode(value).map(Self::$member),)*
                    _ => Err(missing()),
                }
            }
        }
    };
}
abstract_types!(Node: Team,CommitComment,User,Bot, Organization, Repository, Issue, PullRequest, Label, IssueComment,Milestone,Commit,Ref,Blob,Tree,Tag,Reaction,Release,ReleaseAsset,PullRequestReview,PullRequestReviewComment,StatusContext,Status,StatusCheckRollup,ReviewRequest,ProjectV2,ProjectV2Item,ProjectV2Field,ProjectV2SingleSelectField,ProjectV2IterationField,DraftIssue,MergedEvent,ReadyForReviewEvent,ClosedEvent,ReopenedEvent,RenamedTitleEvent,LabeledEvent,UnlabeledEvent,AssignedEvent,UnassignedEvent,MilestonedEvent,DemilestonedEvent,LockedEvent,UnlockedEvent,Discussion,DiscussionCategory,DiscussionComment,Gist);
abstract_types!(Actor: User,Bot);
abstract_types!(Assignee: User);
abstract_types!(IssueOrPullRequest: Issue, PullRequest);
abstract_types!(SearchResultItem: Repository, Issue, PullRequest, Discussion);
abstract_types!(Labelable: Issue, PullRequest, Discussion);
abstract_types!(Assignable: Issue, PullRequest);
abstract_types!(Lockable: Issue, PullRequest);
abstract_types!(RepositoryOwner: User, Organization);
abstract_types!(GitObject: Commit,Blob,Tree,Tag);
abstract_types!(Reactable: Issue,PullRequest,IssueComment,PullRequestReview,PullRequestReviewComment,Release,CommitComment);
abstract_types!(RequestedReviewer: User,Bot,Team);
abstract_types!(StatusCheckRollupContext: StatusContext);
abstract_types!(Starrable: Repository);
abstract_types!(Subscribable: Repository);
abstract_types!(RuleSource: Organization, Repository);

fn field_name(name: &str) -> String {
    let name = name.strip_prefix("r#").unwrap_or(name);
    let mut upper = false;
    name.chars()
        .filter_map(|ch| {
            if ch == '_' {
                upper = true;
                None
            } else if upper {
                upper = false;
                Some(ch.to_ascii_uppercase())
            } else {
                Some(ch)
            }
        })
        .collect()
}

impl Object {
    async fn resolve(
        &self,
        ty: &str,
        field: &str,
        arguments: &Arguments,
        request: &RequestContext,
    ) -> Result<Value> {
        if let Some(repo) = self.0.get("_repo").and_then(Value::as_i64).or_else(|| {
            (ty == "Repository")
                .then(|| self.0["databaseId"].as_i64())
                .flatten()
        }) {
            let scope = match ty {
                "Repository" => match field {
                    "issue" | "issues" | "issueTypes" | "labels" | "milestone" | "milestones"
                    | "assignableUsers" | "mentionableUsers" => "issues",
                    "pullRequest" | "pullRequests" => "pull-requests",
                    "discussion"
                    | "discussions"
                    | "discussionCategories"
                    | "hasDiscussionsEnabled" => "discussions",
                    "defaultBranchRef"
                    | "ref"
                    | "refs"
                    | "object"
                    | "languages"
                    | "primaryLanguage"
                    | "releases"
                    | "release"
                    | "latestRelease"
                    | "isEmpty"
                    | "issueTemplates"
                    | "pullRequestTemplates"
                    | "isBlankIssuesEnabled"
                    | "contactLinks"
                    | "fundingLinks"
                    | "codeOfConduct"
                    | "licenseInfo"
                    | "securityPolicyUrl"
                    | "isSecurityPolicyEnabled" => "contents",
                    _ => "metadata",
                },
                "Issue" => "issues",
                "Label" | "Milestone" => self.0["_scope"].as_str().unwrap_or("issues"),
                "PullRequest" | "PullRequestReview" | "PullRequestReviewComment" => "pull-requests",
                "IssueComment" if self.0["_pull"] == true => "pull-requests",
                "IssueComment" => "issues",
                "StatusContext" | "Status" => "statuses",
                "CheckRun" | "CheckSuite" => "checks",
                "Workflow" | "WorkflowRun" => "actions",
                "Discussion" | "DiscussionComment" | "DiscussionCategory" => "discussions",
                _ => self.0["_scope"].as_str().unwrap_or("contents"),
            };
            request.authorize(repo, scope, false).await?;
        }
        if ty == "Organization" && matches!(field, "team" | "rulesets" | "projects" | "projectsV2" | "projectV2" | "viewerCanCreateProjects") {
            request.authorize_organization(self.0["databaseId"].as_i64().ok_or_else(missing)?).await?;
        }
        if self.0["_ownerKind"] == "org" {
            request.authorize_organization(self.0["_owner"].as_i64().ok_or_else(missing)?).await?;
        }
        if request.bounded()
            && ((ty == "Query" && matches!(field, "user" | "organization" | "repositoryOwner"))
                || (matches!(ty, "User" | "Organization")
                    && matches!(
                        field,
                        "repositories" | "viewerIsFollowing" | "viewerCanFollow"
                    ))
                || (ty == "Repository"
                    && matches!(
                        field,
                        "forks" | "viewerHasStarred" | "viewerSubscription" | "watchers"
                    )))
        {
            return Err(super::token_denied());
        }
        match (ty, field) {
            ("UpdateProjectV2ItemPositionPayload", "items") => boxed(|| projects::resolve("ProjectV2", field, arguments, request, &self.0)).await,
            (_, "repository")
                if matches!(
                    ty,
                    "Blob" | "Tree" | "Tag" | "Ref" | "Milestone" | "Release"
                ) =>
            {
                Ok(repository::object(
                    &repository::by_id(self.0["_repo"].as_i64().ok_or_else(missing)?, request)
                        .await?,
                ))
            }
            ("PullRequestReview" | "PullRequestReviewComment", "reactionGroups") => {
                super::collaboration::reactions(
                    if ty == "PullRequestReview" {
                        db::reactions::Subject::PullRequestReview
                    } else {
                        db::reactions::Subject::PullRequestComment
                    },
                    self.0["databaseId"].as_i64().ok_or_else(missing)?,
                    request,
                )
                .await
            }
            ("CommitComment", _) => match self.0.get(field) {
                Some(value) => Ok(value.clone()),
                None => boxed(|| super::commit_comment::field(field, &self.0, request)).await,
            },
            ("Release", _) => match self.0.get(field) {
                Some(value) => Ok(value.clone()),
                None => boxed(|| super::release::field(field, arguments, &self.0, request)).await,
            },
            ("Tag", "target") => {
                let repo =
                    repository::by_id(self.0["_repo"].as_i64().ok_or_else(missing)?, request)
                        .await?;
                super::git_objects::load(
                    &repo,
                    self.0["_target"].as_str().ok_or_else(missing)?,
                    request,
                )
                .await
            }
            ("TreeEntry", "object") => {
                if self.0["mode"].as_i64() == Some(0o160000) {
                    return Ok(Value::Null);
                }
                super::git_objects::load(
                    &repository::by_id(self.0["_repo"].as_i64().ok_or_else(missing)?, request)
                        .await?,
                    self.0["oid"].as_str().ok_or_else(missing)?,
                    request,
                )
                .await
            }
            ("Query", "viewer" | "user" | "organization" | "repositoryOwner") => {
                boxed(|| user::query(field, arguments, request)).await
            }
            ("Query", "repository") => boxed(|| repository::query(arguments, request)).await,
            ("Query", "search") => {
                if request.bounded() {
                    return Err(super::token_denied());
                }
                request.require_oauth_scope("repo")?;
                let kind = arguments.args.try_get("type")?.enum_name()?;
                let mut value = match kind {
                    "REPOSITORY" => {
                        boxed(|| super::repository_list::search(arguments, request)).await?
                    }
                    "DISCUSSION" => {
                        boxed(|| super::discussions::search(arguments, request)).await?
                    }
                    _ => boxed(|| issue::search(arguments, request)).await?,
                };
                for key in [
                    "codeCount",
                    "discussionCount",
                    "issueCount",
                    "repositoryCount",
                    "userCount",
                    "wikiCount",
                ] {
                    if value.get(key).is_none() {
                        value[key] = json!(0);
                    }
                }
                value["issueSearchType"] = if matches!(kind, "ISSUE" | "ISSUE_ADVANCED") {
                    json!("LEXICAL")
                } else {
                    Value::Null
                };
                value["lexicalFallbackReason"] = Value::Null;
                Ok(value)
            }
            ("User", "organizations") => {
                boxed(|| super::repository_list::organizations(arguments, request, &self.0)).await
            }
            ("User" | "Organization", "repositories") => {
                boxed(|| super::repository_list::list(arguments, request, &self.0)).await
            }
            ("Query", "node") => {
                let id = arg(arguments, "id")?;
                boxed(|| node(&id, request)).await
            }
            ("Query", "rateLimit") => Ok(
                json!({"limit":5000,"cost":1,"remaining":5000-request.rate.used,"used":request.rate.used,"nodeCount":request.nodes.load(Ordering::Relaxed),"resetAt":request.rate.reset_at}),
            ),
            ("Query", "resource") => boxed(|| super::projects::resource(arguments, request)).await,
            ("Mutation", _) if field.contains("ProjectV2") => {
                boxed(|| super::projects::mutation(field, arguments, request)).await
            }
            ("Mutation", "requestReviews" | "rerequestReviews") => {
                boxed(|| super::review_requests::mutation(field, arguments, request)).await
            }
            ("Mutation", _) => boxed(|| super::mutations::resolve(field, arguments, request)).await,
            (ty, _)
                if ty.starts_with("ProjectV2")
                    || ty == "DraftIssue"
                    || (matches!(ty, "User" | "Organization" | "Repository")
                        && matches!(
                            field,
                            "projectV2" | "projects" | "projectsV2" | "viewerCanCreateProjects"
                        )) =>
            {
                boxed(|| super::projects::resolve(ty, field, arguments, request, &self.0)).await
            }
            ("Discussion" | "DiscussionComment" | "DiscussionCategory", _) => {
                boxed(|| super::discussions::resolve(ty, field, arguments, request, &self.0)).await
            }
            ("Gist", "files") => boxed(|| super::gists::files(arguments, request, &self.0)).await,
            ("GistFile", "text") => boxed(|| super::gists::text(arguments, request, &self.0)).await,
            ("Label", "url") => boxed(|| repository::label_url(&self.0, request)).await,
            ("Repository" | "Organization", "rulesets") => {
                boxed(|| super::rulesets::list(ty, arguments, request, &self.0)).await
            }
            ("RepositoryRuleset", "rules") => super::rulesets::rules(arguments, &self.0),
            ("Repository", _) => {
                boxed(|| repository::resolve(field, arguments, request, &self.0)).await
            }
            ("User" | "Organization", _) => {
                boxed(|| user::resolve(ty, field, arguments, request, &self.0)).await
            }
            ("Ref", "branchProtectionRule" | "compare") => {
                boxed(|| super::git_objects::reference_field(field, arguments, &self.0, request))
                    .await
            }
            ("Commit" | "Status" | "StatusContext" | "StatusCheckRollup", _) => {
                boxed(|| super::git::resolve(ty, field, arguments, request, &self.0)).await
            }
            ("IssueComment", _) => {
                boxed(|| collaboration::resolve_comment(field, request, &self.0)).await
            }
            ("Issue" | "PullRequest", _) => {
                boxed(|| issue::resolve(ty, field, arguments, request, &self.0)).await
            }
            _ => {
                self.0.get(field).cloned().ok_or_else(|| {
                    Error::new(format!("Codeyard does not support {ty}.{field} yet"))
                })
            }
        }
    }
}

// Signatures are checked against the generated traits. Arguments remain typed until
// they cross into the family functions, preserving SDL defaults and omitted inputs.
macro_rules! fields {
    ($ty:literal; $($method:ident($($arg:ident: $input:ty),*) -> $output:ty;)* ) => {$(
        fn $method<'a>(&'a self, ctx: &'a Context<'_>, $($arg: $input),*) -> gql::FieldFuture<'a, $output> {
            Box::pin(async move {
                let arguments = Arguments::new([$( (field_name(stringify!($arg)), InputType::to_value(&$arg)), )*]).with_selection(ctx);
                let field = field_name(stringify!($method));
                let request = ctx.data::<RequestContext>()?;
                let value = self.resolve($ty, &field, &arguments, request).await?;
                request.charge_value(&value)?;
                Decode::decode(value)
            })
        }
    )*};
}

impl gql::QueryResolver for Object {
    fields!("Query";
        viewer() -> gql::User;
        repository_owner(login: String) -> Option<gql::RepositoryOwner>;
        user(login: String) -> Option<gql::User>;
        organization(login: String) -> Option<gql::Organization>;
        repository(follow_renames: Option<bool>, name: String, owner: String) -> Option<gql::Repository>;
        node(id: ID) -> Option<gql::Node>;
        resource(url: gql::URI) -> Option<gql::UniformResourceLocatable>;
        rate_limit(dry_run: Option<bool>) -> Option<gql::RateLimit>;
        search(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,query:String,r#type:gql::SearchType) -> gql::SearchResultItemConnection;
    );

    fn nodes<'a>(
        &'a self,
        ctx: &'a Context<'_>,
        ids: Vec<ID>,
    ) -> gql::FieldFuture<'a, Vec<Option<gql::Node>>> {
        Box::pin(async move {
            if ids.len() > 100 {
                return Err(Error::new("At most 100 node IDs are allowed"));
            }
            let request = ctx.data::<RequestContext>()?;
            let mut nodes = Vec::with_capacity(ids.len());
            for (index, id) in ids.iter().enumerate() {
                match node(id.as_str(), request).await.and_then(|value| {
                    request.charge_value(&value)?;
                    gql::Node::decode(value)
                }) {
                    Ok(node) => nodes.push(Some(node)),
                    Err(error) => {
                        let mut error = ctx.set_error_path(error.into_server_error(ctx.item.pos));
                        error.path.push(async_graphql::PathSegment::Index(index));
                        ctx.add_error(error);
                        nodes.push(None);
                    }
                }
            }
            Ok(nodes)
        })
    }
}

macro_rules! issue_fields {
    ($ty:literal) => {
        fields!($ty;
            id() -> ID; database_id() -> Option<i32>; number() -> i32; title() -> String; body() -> String;
            created_at() -> gql::DateTime; closed_at() -> Option<gql::DateTime>; author() -> Option<gql::Actor>; url() -> gql::URI;
            labels(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::LabelOrder>>) -> Option<gql::LabelConnection>;
            comments(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::IssueCommentOrder>>) -> gql::IssueCommentConnection;
        );
    };
}
impl gql::LabelResolver for Object {
    fields!("Label"; id() -> ID; name() -> String; color() -> String; description() -> Option<String>; created_at() -> Option<gql::DateTime>; updated_at() -> Option<gql::DateTime>; is_default() -> bool; url() -> gql::URI;);
}
impl gql::RefResolver for Object {
    fields!("Ref"; id() -> ID; name() -> String; prefix()->String; repository()->gql::Repository; target() -> Option<gql::GitObject>;
        branch_protection_rule() -> Option<gql::BranchProtectionRule>;
        compare(head_ref: String) -> Option<gql::Comparison>;
    );
}
impl gql::RateLimitResolver for Object {
    fields!("RateLimit"; limit() -> i32; cost() -> i32; remaining() -> i32; used() -> i32; node_count() -> i32; reset_at() -> gql::DateTime;);
}
impl gql::PageInfoResolver for Object {
    fields!("PageInfo"; has_next_page() -> bool; has_previous_page() -> bool; start_cursor() -> Option<String>; end_cursor() -> Option<String>;);
}

macro_rules! connection {
    ($resolver:ident, $edge_resolver:ident, $ty:literal, $edge_ty:literal, $node:ident, $edge:ident) => {
        impl gql::$resolver for Object {
            fields!($ty;
                nodes() -> Option<Vec<Option<gql::$node>>>; edges() -> Option<Vec<Option<gql::$edge>>>;
                page_info() -> gql::PageInfo; total_count() -> i32;
            );
        }
        impl gql::$edge_resolver for Object {
            fields!($edge_ty; cursor() -> String; node() -> Option<gql::$node>;);
        }
    };
}
connection!(
    LabelConnectionResolver,
    LabelEdgeResolver,
    "LabelConnection",
    "LabelEdge",
    Label,
    LabelEdge
);
connection!(
    IssueConnectionResolver,
    IssueEdgeResolver,
    "IssueConnection",
    "IssueEdge",
    Issue,
    IssueEdge
);
connection!(
    PullRequestConnectionResolver,
    PullRequestEdgeResolver,
    "PullRequestConnection",
    "PullRequestEdge",
    PullRequest,
    PullRequestEdge
);
connection!(
    IssueCommentConnectionResolver,
    IssueCommentEdgeResolver,
    "IssueCommentConnection",
    "IssueCommentEdge",
    IssueComment,
    IssueCommentEdge
);

macro_rules! issue_payload {
    ($resolver:ident, $ty:literal) => {
        impl gql::$resolver for Object {
            fields!($ty; client_mutation_id() -> Option<String>; issue() -> Option<gql::Issue>;);
        }
    };
}
issue_payload!(CreateIssuePayloadResolver, "CreateIssuePayload");
issue_payload!(CloseIssuePayloadResolver, "CloseIssuePayload");
issue_payload!(ReopenIssuePayloadResolver, "ReopenIssuePayload");
mod collaboration_fields;
mod discussions;
mod git_fields;
mod issue_bindings;
mod mutation_fields;
mod node_fields;
mod projects;
mod repository_fields;
mod repository_metadata;
mod search_fields;

impl gql::AddCommentPayloadResolver for Object {
    fields!("AddCommentPayload"; client_mutation_id() -> Option<String>; comment_edge() -> Option<gql::IssueCommentEdge>; subject() -> Option<gql::Node>;);
}
