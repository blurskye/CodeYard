//! The fields selected by cli/cli's api/query_builder.go and repository commands.
use super::*;

macro_rules! account_fields {
    ($ty:literal) => {
        fields!($ty;
            id() -> ID; database_id() -> Option<i32>; login() -> String; name() -> Option<String>;
            url() -> gql::URI; avatar_url(size: Option<i32>) -> gql::URI; created_at() -> gql::DateTime;
            repositories(affiliations: Option<Vec<Option<gql::RepositoryAffiliation>>>, after: Option<String>, before: Option<String>, first: Option<i32>, has_issues_enabled: Option<bool>, is_archived: Option<bool>, is_fork: Option<bool>, is_locked: Option<bool>, last: Option<i32>, order_by: Option<gql::Input<gql::RepositoryOrder>>, owner_affiliations: Option<Vec<Option<gql::RepositoryAffiliation>>>, privacy: Option<gql::RepositoryPrivacy>, visibility: Option<gql::RepositoryVisibility>) -> gql::RepositoryConnection;
        );
    };
}
impl gql::UserResolver for Object {
    account_fields!("User");
    projects::owner_fields!("User");
    fields!("User"; organizations(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::OrganizationOrder>>) -> gql::OrganizationConnection;);
    fields!("User"; gists(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::GistOrder>>, privacy: Option<gql::GistPrivacy>) -> gql::GistConnection;);
    fields!("User"; email() -> String; bio() -> Option<String>; company() -> Option<String>; location() -> Option<String>; website_url() -> Option<gql::URI>; is_viewer() -> bool; viewer_can_follow() -> bool; viewer_is_following() -> bool;);
}
impl gql::OrganizationResolver for Object {
    fields!("Organization";team(slug:String)->Option<gql::Team>;);
    account_fields!("Organization");
    projects::owner_fields!("Organization");
    projects::classic_fields!("Organization");
    fields!("Organization"; email() -> Option<String>; description() -> Option<String>; website_url() -> Option<gql::URI>;);
    fields!("Organization"; rulesets(after: Option<String>, before: Option<String>, first: Option<i32>, include_parents: Option<bool>, last: Option<i32>, targets: Option<Vec<gql::RepositoryRulesetTarget>>) -> Option<gql::RepositoryRulesetConnection>;);
}
impl gql::RepositoryResolver for Object {
    discussions::repository_fields!();
    fields!("Repository";
        id() -> ID; database_id() -> Option<i32>; name() -> String; description() -> Option<String>;
        is_private() -> bool; is_fork() -> bool; is_archived() -> bool; visibility() -> gql::RepositoryVisibility;
        created_at() -> gql::DateTime; updated_at() -> gql::DateTime; stargazer_count() -> i32; fork_count() -> i32;
        owner() -> gql::RepositoryOwner; name_with_owner() -> String; url() -> gql::URI; ssh_url() -> gql::GitSSHRemote;
        pushed_at() -> Option<gql::DateTime>; default_branch_ref() -> Option<gql::Ref>;
        milestone(number:i32)->Option<gql::Milestone>;
        issue_types(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::IssueTypeOrder>>) -> Option<gql::IssueTypeConnection>;
        milestones(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::MilestoneOrder>>,query:Option<String>,states:Option<Vec<gql::MilestoneState>>)->Option<gql::MilestoneConnection>;
        latest_release()->Option<gql::Release>;
        release(tag_name:String)->Option<gql::Release>;
        releases(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::ReleaseOrder>>)->gql::ReleaseConnection;
        r#ref(qualified_name:String)->Option<gql::Ref>;
        object(expression:Option<String>,oid:Option<gql::GitObjectID>)->Option<gql::GitObject>;
        refs(after:Option<String>,before:Option<String>,direction:Option<gql::OrderDirection>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::RefOrder>>,query:Option<String>,ref_prefix:String)->Option<gql::RefConnection>;
        issue(number: i32) -> Option<gql::Issue>; pull_request(number: i32) -> Option<gql::PullRequest>;
        issue_or_pull_request(number: i32) -> Option<gql::IssueOrPullRequest>;
        issues(after: Option<String>, before: Option<String>, filter_by: Option<gql::Input<gql::IssueFilters>>, first: Option<i32>, labels: Option<Vec<String>>, last: Option<i32>, order_by: Option<gql::Input<gql::IssueOrder>>, states: Option<Vec<gql::IssueState>>) -> gql::IssueConnection;
        pull_requests(after: Option<String>, base_ref_name: Option<String>, before: Option<String>, first: Option<i32>, head_ref_name: Option<String>, labels: Option<Vec<String>>, last: Option<i32>, order_by: Option<gql::Input<gql::IssueOrder>>, states: Option<Vec<gql::PullRequestState>>) -> gql::PullRequestConnection;
        labels(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::LabelOrder>>, query: Option<String>) -> Option<gql::LabelConnection>;
        homepage_url() -> Option<gql::URI>; disk_usage() -> Option<i32>; archived_at() -> Option<gql::DateTime>;
        is_empty() -> bool; is_in_organization() -> bool; is_template() -> bool; is_mirror() -> bool;
        has_issues_enabled() -> bool; has_projects_enabled() -> bool; has_wiki_enabled() -> bool;
        parent() -> Option<gql::Repository>; template_repository() -> Option<gql::Repository>; mirror_url() -> Option<gql::URI>;
        merge_commit_allowed() -> bool; squash_merge_allowed() -> bool; rebase_merge_allowed() -> bool; delete_branch_on_merge() -> bool;
        viewer_can_administer() -> bool; viewer_permission() -> Option<gql::RepositoryPermission>; viewer_has_starred() -> bool;
        viewer_subscription() -> Option<gql::SubscriptionState>; viewer_default_commit_email() -> Option<String>;
        viewer_possible_commit_emails() -> Option<Vec<String>>; viewer_default_merge_method() -> gql::PullRequestMergeMethod;
        repository_topics(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::RepositoryTopicConnection;
        primary_language() -> Option<gql::Language>;
        languages(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::LanguageOrder>>) -> Option<gql::LanguageConnection>;
        watchers(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::UserConnection;
        forks(affiliations: Option<Vec<Option<gql::RepositoryAffiliation>>>, after: Option<String>, before: Option<String>, first: Option<i32>, has_issues_enabled: Option<bool>, is_locked: Option<bool>, last: Option<i32>, order_by: Option<gql::Input<gql::RepositoryOrder>>, owner_affiliations: Option<Vec<Option<gql::RepositoryAffiliation>>>, privacy: Option<gql::RepositoryPrivacy>, visibility: Option<gql::RepositoryVisibility>) -> gql::RepositoryConnection;
        projects(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::ProjectOrder>>, search: Option<String>, states: Option<Vec<gql::ProjectState>>) -> gql::ProjectConnection;
        projects_v2(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, min_permission_level: Option<gql::ProjectV2PermissionLevel>, order_by: Option<gql::Input<gql::ProjectV2Order>>, query: Option<String>) -> gql::ProjectV2Connection;
        uses_custom_open_graph_image() -> bool; open_graph_image_url() -> gql::URI;
        security_policy_url() -> Option<gql::URI>; is_security_policy_enabled() -> Option<bool>; is_blank_issues_enabled() -> bool;
        contact_links() -> Option<Vec<gql::RepositoryContactLink>>; funding_links() -> Vec<gql::FundingLink>;
        code_of_conduct() -> Option<gql::CodeOfConduct>; license_info() -> Option<gql::License>;
        auto_merge_allowed() -> bool; is_user_configuration_repository() -> bool;
        issue_templates() -> Option<Vec<gql::IssueTemplate>>; pull_request_templates() -> Option<Vec<gql::PullRequestTemplate>>;
        assignable_users(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, query: Option<String>) -> gql::UserConnection;
        mentionable_users(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, query: Option<String>) -> gql::UserConnection;
        rulesets(after: Option<String>, before: Option<String>, first: Option<i32>, include_parents: Option<bool>, last: Option<i32>, targets: Option<Vec<gql::RepositoryRulesetTarget>>) -> Option<gql::RepositoryRulesetConnection>;
    );
}
connection!(
    RepositoryConnectionResolver,
    RepositoryEdgeResolver,
    "RepositoryConnection",
    "RepositoryEdge",
    Repository,
    RepositoryEdge
);
connection!(
    UserConnectionResolver,
    UserEdgeResolver,
    "UserConnection",
    "UserEdge",
    User,
    UserEdge
);
connection!(
    RepositoryTopicConnectionResolver,
    RepositoryTopicEdgeResolver,
    "RepositoryTopicConnection",
    "RepositoryTopicEdge",
    RepositoryTopic,
    RepositoryTopicEdge
);
connection!(
    GistConnectionResolver,
    GistEdgeResolver,
    "GistConnection",
    "GistEdge",
    Gist,
    GistEdge
);
impl gql::GistResolver for Object {
    fields!("Gist"; id() -> ID; name() -> String; description() -> Option<String>; is_public() -> bool; updated_at() -> gql::DateTime; files(limit: Option<i32>, oid: Option<gql::GitObjectID>) -> Option<Vec<Option<gql::GistFile>>>;);
}
impl gql::GistFileResolver for Object {
    fields!("GistFile"; name() -> Option<String>; text(truncate: Option<i32>) -> Option<String>;);
}
impl gql::RepositoryTopicResolver for Object {
    fields!("RepositoryTopic"; topic() -> gql::Topic;);
}
impl gql::TopicResolver for Object {
    fields!("Topic"; name() -> String;);
}
objects!(
    RepositoryRuleset,
    RepositoryRulesetConnection,
    RepositoryRulesetEdge,
    RepositoryRule,
    RepositoryRuleConnection,
    RepositoryRuleEdge
);
impl gql::RepositoryRulesetResolver for Object {
    fields!("RepositoryRuleset";
        id() -> ID; database_id() -> Option<i32>; name() -> String;
        target() -> Option<gql::RepositoryRulesetTarget>; enforcement() -> gql::RuleEnforcement;
        source() -> gql::RuleSource; created_at() -> gql::DateTime; updated_at() -> gql::DateTime;
        rules(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, r#type: Option<gql::RepositoryRuleType>) -> Option<gql::RepositoryRuleConnection>;
    );
}
impl gql::RepositoryRuleResolver for Object {
    fields!("RepositoryRule"; r#type() -> gql::RepositoryRuleType;);
}
connection!(
    RepositoryRulesetConnectionResolver,
    RepositoryRulesetEdgeResolver,
    "RepositoryRulesetConnection",
    "RepositoryRulesetEdge",
    RepositoryRuleset,
    RepositoryRulesetEdge
);
connection!(
    RepositoryRuleConnectionResolver,
    RepositoryRuleEdgeResolver,
    "RepositoryRuleConnection",
    "RepositoryRuleEdge",
    RepositoryRule,
    RepositoryRuleEdge
);
