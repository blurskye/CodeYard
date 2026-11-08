//! Where every table's rows belong, so an account move (architecture stage 6) knows which
//! rows to copy and which writes its freeze refuses. A row belongs to the first non-null
//! reference in its rule: an account, a repository (its owner's), or a parent row. Rows whose
//! references are all null stay with the cell. `Cell` tables hold this cell's machines,
//! queues, limits and caches and never move; `Shared` tables hold reference data or content
//! that any cell may hold, and a move copies only the rows its moved rows point at. The tests
//! below fail when a table in either schema has no rule, a rule names a missing column or
//! table, or the PostgreSQL baseline's move guard for a table differs from its rule.
use codeyard_types::cells::ObjectKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reference {
    /// A user id.
    User(&'static str),
    /// An organization id.
    Organization(&'static str),
    /// An account id whose kind (`user` or `org`) is in the first column.
    Owner(&'static str, &'static str),
    /// A repository id; the repository's owner decides.
    Repository(&'static str),
    /// The id of a row in another table, whose rule decides.
    Parent(&'static str, &'static str),
}

impl Reference {
    pub fn column(self) -> &'static str {
        match self {
            User(column)
            | Organization(column)
            | Owner(_, column)
            | Repository(column)
            | Parent(column, _) => column,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rule {
    Owned(&'static [Reference]),
    Cell,
    Shared,
}

use Reference::{Organization, Owner, Parent, Repository, User};
use Rule::{Cell, Owned, Shared};

pub const TABLES: &[(&str, Rule)] = &[
    ("account_moves", Cell),
    (
        "action_runner_group_repositories",
        Owned(&[Repository("repo_id")]),
    ),
    ("action_runner_groups", Owned(&[Organization("org_id")])),
    (
        "action_runner_remove_tokens",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    (
        "action_runner_tokens",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    (
        "action_runners",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    (
        "actions_blob_grants",
        Owned(&[Parent("object_id", "actions_objects")]),
    ),
    (
        "actions_blob_parts",
        Owned(&[Parent("object_id", "actions_objects")]),
    ),
    (
        "actions_concurrency_groups",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "actions_deployment_reviews",
        Owned(&[Parent("run_id", "ci_runs")]),
    ),
    (
        "actions_github_tokens",
        Owned(&[Parent("run_id", "ci_runs")]),
    ),
    (
        "actions_job_annotations",
        Owned(&[Parent("job_id", "actions_jobs")]),
    ),
    ("actions_job_concurrency", Owned(&[Repository("repo_id")])),
    ("actions_job_states", Owned(&[Parent("run_id", "ci_runs")])),
    ("actions_jobs", Owned(&[Parent("run_id", "ci_runs")])),
    ("actions_log_downloads", Owned(&[Repository("repo_id")])),
    ("actions_objects", Owned(&[Repository("repo_id")])),
    (
        "actions_oidc_jobs",
        Owned(&[Parent("token_id", "personal_access_tokens")]),
    ),
    ("actions_oidc_key", Cell),
    (
        "actions_oidc_org_settings",
        Owned(&[Organization("org_id")]),
    ),
    (
        "actions_oidc_properties",
        Owned(&[Parent(
            "definition_id",
            "organization_repository_properties",
        )]),
    ),
    (
        "actions_oidc_repo_settings",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "actions_policies",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    ("actions_replays", Owned(&[Parent("run_id", "ci_runs")])),
    ("actions_run_concurrency", Owned(&[Repository("repo_id")])),
    (
        "actions_run_metadata",
        Owned(&[Parent("run_id", "ci_runs")]),
    ),
    (
        "actions_runtime_tokens",
        Owned(&[Parent("run_id", "ci_runs")]),
    ),
    ("actions_settings", Cell),
    (
        "actions_step_summaries",
        Owned(&[Parent("job_id", "actions_jobs")]),
    ),
    ("actions_steps", Owned(&[Parent("job_id", "actions_jobs")])),
    ("actions_workflows", Owned(&[Repository("repo_id")])),
    (
        "activity_feed_tokens",
        Owned(&[Parent("api_token_id", "personal_access_tokens")]),
    ),
    ("anonymous_sessions", Cell),
    ("artifact_attestations", Owned(&[Repository("repo_id")])),
    ("artifact_metadata", Owned(&[Repository("repo_id")])),
    ("artifact_metadata_jobs", Owned(&[Organization("org_id")])),
    (
        "artifact_metadata_records",
        Owned(&[Organization("org_id")]),
    ),
    ("attestation_bundles", Shared),
    (
        "attestation_subjects",
        Owned(&[Parent("attestation_id", "artifact_attestations")]),
    ),
    ("audit_log", Owned(&[Organization("org_id")])),
    ("auth_login_limits", Cell),
    ("branch_protection_rules", Owned(&[Repository("repo_id")])),
    ("branch_redirects", Owned(&[Repository("repo_id")])),
    ("cell_outbox", Cell),
    ("cell_prefixes", Cell),
    ("cell_presence", Owned(&[Organization("organization_id")])),
    ("cell_region_policy", Cell),
    ("cell_request_counts", Cell),
    (
        "check_annotations",
        Owned(&[Parent("run_id", "check_runs")]),
    ),
    ("check_run_identities", Shared),
    ("check_runs", Owned(&[Repository("repo_id")])),
    ("check_suite_preferences", Owned(&[Repository("repo_id")])),
    ("check_suites", Owned(&[Repository("repo_id")])),
    ("ci_accepted_pushes", Owned(&[Repository("repo_id")])),
    ("ci_deployments", Owned(&[Repository("repo_id")])),
    ("ci_log_chunks", Owned(&[Parent("run_id", "ci_runs")])),
    ("ci_runs", Owned(&[Repository("repo_id")])),
    ("ci_schedule_runs", Owned(&[Repository("repo_id")])),
    ("cluster_leases", Cell),
    ("cluster_scaling_policies", Cell),
    ("cluster_ssh_host_key", Cell),
    (
        "code_scanning_alert_counters",
        Owned(&[Repository("repo_id")]),
    ),
    ("code_scanning_alerts", Owned(&[Repository("repo_id")])),
    ("code_scanning_analyses", Owned(&[Repository("repo_id")])),
    (
        "code_scanning_analysis_findings",
        Owned(&[Parent("analysis_id", "code_scanning_analyses")]),
    ),
    (
        "code_scanning_instances",
        Owned(&[Parent("alert_id", "code_scanning_alerts")]),
    ),
    ("code_scanning_uploads", Owned(&[Repository("repo_id")])),
    (
        "code_security_configurations",
        Owned(&[Organization("org_id")]),
    ),
    ("codeql_databases", Owned(&[Repository("repo_id")])),
    ("commit_comments", Owned(&[Repository("repo_id")])),
    ("commit_status_history", Owned(&[Repository("repo_id")])),
    ("commit_statuses", Owned(&[Repository("repo_id")])),
    ("dependabot_alerts", Owned(&[Repository("repo_id")])),
    ("dependency_sbom_reports", Owned(&[Repository("repo_id")])),
    ("dependency_snapshots", Owned(&[Repository("repo_id")])),
    ("deploy_keys", Owned(&[Repository("repo_id")])),
    (
        "deployment_branch_policies",
        Owned(&[Parent("environment_id", "deployment_environments")]),
    ),
    ("deployment_environments", Owned(&[Repository("repo_id")])),
    (
        "deployment_protection_gates",
        Owned(&[Parent("run_id", "ci_runs")]),
    ),
    (
        "deployment_protection_rules",
        Owned(&[Parent("environment_id", "deployment_environments")]),
    ),
    (
        "deployment_statuses",
        Owned(&[Parent("deployment_id", "repository_deployments")]),
    ),
    ("discussion_categories", Owned(&[Repository("repo_id")])),
    (
        "discussion_comments",
        Owned(&[Parent("discussion_id", "discussions")]),
    ),
    (
        "discussion_labels",
        Owned(&[Parent("discussion_id", "discussions")]),
    ),
    ("discussions", Owned(&[Repository("repo_id")])),
    ("download_rate_limit", Cell),
    ("email_outbox", Cell),
    ("email_settings", Cell),
    ("email_tokens", Owned(&[User("user_id")])),
    ("events", Owned(&[Repository("repo_id")])),
    ("file_reason_limits", Cell),
    ("files", Shared),
    (
        "fine_grained_token_repositories",
        Owned(&[Parent("token_id", "fine_grained_tokens")]),
    ),
    (
        "fine_grained_tokens",
        Owned(&[Parent("token_id", "personal_access_tokens")]),
    ),
    ("follows", Owned(&[User("following_id")])),
    ("gist_comments", Owned(&[Repository("repo_id")])),
    ("gist_identifiers", Shared),
    ("gist_stars", Owned(&[Repository("repo_id")])),
    ("gists", Owned(&[Repository("repo_id")])),
    ("github_api_limits", Cell),
    (
        "github_app_callbacks",
        Owned(&[Parent("app_id", "github_apps")]),
    ),
    (
        "github_app_deliveries",
        Owned(&[Parent("webhook_id", "github_app_hooks")]),
    ),
    (
        "github_app_hooks",
        Owned(&[Parent("app_id", "github_apps")]),
    ),
    (
        "github_app_installations",
        Owned(&[Organization("org_id"), User("user_id")]),
    ),
    ("github_app_manifests", Owned(&[User("user_id")])),
    (
        "github_app_repositories",
        Owned(&[Parent("installation_id", "github_app_installations")]),
    ),
    (
        "github_app_requests",
        Owned(&[Organization("org_id"), User("user_id")]),
    ),
    (
        "github_app_tokens",
        Owned(&[Parent("token_id", "personal_access_tokens")]),
    ),
    ("github_apps", Owned(&[User("owner_id")])),
    ("global_advisories", Shared),
    ("gpg_keys", Owned(&[User("user_id")])),
    ("graphql_rate_limits", Cell),
    ("identity_heartbeat", Cell),
    (
        "immutable_release_owner_names",
        Owned(&[Owner("owner_kind", "owner_id")]),
    ),
    (
        "immutable_release_publications",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "immutable_release_repository_names",
        Owned(&[Repository("repo_id")]),
    ),
    ("interaction_contributors", Owned(&[Repository("repo_id")])),
    (
        "interaction_limits",
        Owned(&[
            Repository("repo_id"),
            Organization("org_id"),
            User("user_id"),
        ]),
    ),
    ("interaction_pull_bypass", Owned(&[Repository("repo_id")])),
    (
        "interaction_pull_caps",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    ("issue_assignees", Owned(&[Parent("issue_id", "issues")])),
    ("issue_attribute_guard", Cell),
    (
        "issue_attribute_options",
        Owned(&[Parent("attribute_id", "issue_attributes")]),
    ),
    (
        "issue_attributes",
        Owned(&[User("owner_user_id"), Organization("owner_org_id")]),
    ),
    ("issue_comment_pins", Owned(&[Parent("issue_id", "issues")])),
    ("issue_comments", Owned(&[Parent("issue_id", "issues")])),
    ("issue_duplicates", Owned(&[Parent("issue_id", "issues")])),
    ("issue_field_values", Owned(&[Parent("issue_id", "issues")])),
    ("issue_labels", Owned(&[Parent("issue_id", "issues")])),
    ("issue_linked_branches", Owned(&[Repository("repo_id")])),
    ("issue_redirects", Owned(&[Repository("repo_id")])),
    ("issue_relation_guard", Cell),
    ("issue_relations", Owned(&[Parent("source_id", "issues")])),
    ("issue_suggestions", Owned(&[Parent("issue_id", "issues")])),
    (
        "issue_type_assignments",
        Owned(&[Parent("issue_id", "issues")]),
    ),
    ("issues", Owned(&[Repository("repo_id")])),
    ("join_tokens", Cell),
    ("labels", Owned(&[Repository("repo_id")])),
    ("milestones", Owned(&[Repository("repo_id")])),
    ("moved_credentials", Cell),
    ("node_configuration", Cell),
    ("node_drain_repositories", Cell),
    ("nodes", Cell),
    ("notification_events", Cell),
    (
        "notification_thread_subscriptions",
        Owned(&[Parent("thread_id", "notification_threads")]),
    ),
    ("notification_threads", Owned(&[User("user_id")])),
    ("notifications", Owned(&[User("user_id")])),
    ("oauth_accounts", Owned(&[User("user_id")])),
    ("oauth_app_grants", Owned(&[User("user_id")])),
    (
        "oauth_app_tokens",
        Owned(&[Parent("token_id", "personal_access_tokens")]),
    ),
    ("oauth_applications", Owned(&[User("owner_id")])),
    ("oauth_states", Cell),
    ("org_actions_keys", Owned(&[Organization("org_id")])),
    ("org_actions_settings", Owned(&[Organization("org_id")])),
    (
        "org_actions_value_repositories",
        Owned(&[Repository("repo_id")]),
    ),
    ("org_actions_values", Owned(&[Organization("org_id")])),
    ("org_dependabot_access", Owned(&[Organization("org_id")])),
    (
        "org_dependabot_access_repositories",
        Owned(&[Repository("repo_id")]),
    ),
    ("org_dependabot_keys", Owned(&[Organization("org_id")])),
    (
        "org_dependabot_secret_repositories",
        Owned(&[Repository("repo_id")]),
    ),
    ("org_dependabot_secrets", Owned(&[Organization("org_id")])),
    ("org_memberships", Owned(&[Organization("org_id")])),
    ("org_private_registries", Owned(&[Organization("org_id")])),
    ("organization_blocks", Owned(&[Organization("org_id")])),
    (
        "organization_external_property_installations",
        Owned(&[Organization("org_id")]),
    ),
    (
        "organization_external_property_values",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "organization_immutable_release_repositories",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "organization_immutable_releases",
        Owned(&[Organization("org_id")]),
    ),
    ("organization_invitations", Owned(&[Organization("org_id")])),
    (
        "organization_member_history",
        Owned(&[Organization("org_id")]),
    ),
    (
        "organization_repository_properties",
        Owned(&[Organization("org_id")]),
    ),
    (
        "organization_role_assignments",
        Owned(&[Organization("org_id")]),
    ),
    ("organization_roles", Shared),
    ("organization_settings", Owned(&[Organization("org_id")])),
    (
        "organization_token_policies",
        Owned(&[Organization("org_id")]),
    ),
    ("organizations", Owned(&[Organization("id")])),
    ("osv_advisories", Shared),
    ("osv_packages", Shared),
    (
        "package_blob_grants",
        Owned(&[Parent("package_id", "packages")]),
    ),
    ("package_blobs", Shared),
    ("package_files", Owned(&[Parent("package_id", "packages")])),
    (
        "package_registry_tokens",
        Owned(&[Parent("token_id", "personal_access_tokens")]),
    ),
    ("package_tags", Owned(&[Parent("package_id", "packages")])),
    (
        "package_uploads",
        Owned(&[Parent("package_id", "packages")]),
    ),
    (
        "package_version_blobs",
        Owned(&[Parent("version_id", "package_versions")]),
    ),
    (
        "package_versions",
        Owned(&[Parent("package_id", "packages")]),
    ),
    (
        "packages",
        Owned(&[User("owner_user_id"), Organization("owner_org_id")]),
    ),
    ("pages_builds", Owned(&[Repository("id")])),
    ("pages_deployments", Owned(&[Repository("id")])),
    ("pages_sites", Owned(&[Repository("repo_id")])),
    ("pending_object_deletions", Cell),
    ("personal_access_tokens", Owned(&[User("user_id")])),
    ("pinned_issues", Owned(&[Repository("repo_id")])),
    (
        "pr_assignees",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_comment_pins",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_comments",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_labels",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_reviewers",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_reviews",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    (
        "pr_team_reviewers",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    ("project_fields", Owned(&[Parent("project_id", "projects")])),
    ("project_items", Owned(&[Repository("repo_id")])),
    ("project_repositories", Owned(&[Repository("repo_id")])),
    ("project_teams", Owned(&[Parent("project_id", "projects")])),
    ("project_views", Owned(&[Parent("project_id", "projects")])),
    (
        "projects",
        Owned(&[User("owner_user_id"), Organization("owner_org_id")]),
    ),
    ("pull_request_async_merges", Owned(&[Repository("repo_id")])),
    (
        "pull_request_auto_merges",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    ("pull_request_stacks", Owned(&[Repository("repo_id")])),
    (
        "pull_request_viewed_files",
        Owned(&[Parent("pull_request_id", "pull_requests")]),
    ),
    ("pull_requests", Owned(&[Repository("repo_id")])),
    ("reactions", Owned(&[Repository("repo_id")])),
    (
        "registry_namespaces",
        Owned(&[User("owner_user_id"), Organization("owner_org_id")]),
    ),
    ("release_assets", Owned(&[Parent("release_id", "releases")])),
    (
        "release_discussions",
        Owned(&[Parent("release_id", "releases")]),
    ),
    ("releases", Owned(&[Repository("repo_id")])),
    ("remote_pointers", Owned(&[User("user_id")])),
    ("remote_pull_requests", Owned(&[Repository("base_repo_id")])),
    ("repo_access", Owned(&[Repository("repo_id")])),
    ("repo_actions_keys", Owned(&[Repository("repo_id")])),
    ("repo_actions_settings", Owned(&[Repository("repo_id")])),
    ("repo_actions_variables", Owned(&[Repository("repo_id")])),
    ("repo_dependabot_keys", Owned(&[Repository("repo_id")])),
    ("repo_dependabot_secrets", Owned(&[Repository("repo_id")])),
    ("repo_environment_values", Owned(&[Repository("repo_id")])),
    ("repo_language_cache", Cell),
    ("repo_languages", Owned(&[Repository("repo_id")])),
    ("repo_secrets", Owned(&[Repository("repo_id")])),
    ("repo_storage_history", Cell),
    ("repositories", Owned(&[Repository("id")])),
    ("repository_advisories", Owned(&[Repository("repo_id")])),
    ("repository_autolinks", Owned(&[Repository("repo_id")])),
    ("repository_committed_refs", Cell),
    (
        "repository_dependency_security",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_deployments", Owned(&[Repository("repo_id")])),
    (
        "repository_discussion_counters",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "repository_discussion_settings",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "repository_exports",
        Owned(&[User("owner_user_id"), Organization("owner_org_id")]),
    ),
    ("repository_features", Owned(&[Repository("repo_id")])),
    (
        "repository_immutable_releases",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_import_authors", Owned(&[Repository("repo_id")])),
    ("repository_imports", Owned(&[Repository("repo_id")])),
    ("repository_invitations", Owned(&[Repository("repo_id")])),
    (
        "repository_issue_suggestions",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_merge_epochs", Owned(&[Repository("repo_id")])),
    (
        "repository_migration_locks",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_move_nodes", Cell),
    ("repository_moves", Cell),
    (
        "repository_private_reporting",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "repository_property_values",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_pushes", Owned(&[Repository("repo_id")])),
    (
        "repository_redirects",
        Owned(&[Owner("owner_kind", "owner_id")]),
    ),
    ("repository_ref_activities", Owned(&[Repository("repo_id")])),
    ("repository_replicas", Cell),
    (
        "repository_rule_suites",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    (
        "repository_ruleset_versions",
        Owned(&[Parent("ruleset_id", "repository_rulesets")]),
    ),
    (
        "repository_rulesets",
        Owned(&[Repository("owner_repo_id"), Organization("owner_org_id")]),
    ),
    (
        "repository_security_replies",
        Owned(&[Parent("report_id", "repository_security_reports")]),
    ),
    (
        "repository_security_reports",
        Owned(&[Repository("repo_id")]),
    ),
    (
        "repository_security_settings",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_star_days", Owned(&[Repository("repo_id")])),
    ("repository_topics", Owned(&[Repository("repo_id")])),
    ("repository_traffic_counts", Owned(&[Repository("repo_id")])),
    ("repository_traffic_keys", Owned(&[Repository("repo_id")])),
    (
        "repository_traffic_visitors",
        Owned(&[Repository("repo_id")]),
    ),
    ("repository_voters", Cell),
    ("repository_write_control", Cell),
    ("repository_write_refs", Cell),
    ("repository_write_votes", Cell),
    ("repository_writes", Cell),
    ("secret_scanning_alerts", Owned(&[Repository("repo_id")])),
    ("secret_scanning_bypasses", Owned(&[Repository("repo_id")])),
    ("secret_scanning_commits", Owned(&[Repository("repo_id")])),
    ("secret_scanning_jobs", Owned(&[Repository("repo_id")])),
    ("secret_scanning_locations", Owned(&[Repository("repo_id")])),
    (
        "secret_scanning_pattern_configs",
        Owned(&[Organization("org_id")]),
    ),
    (
        "secret_scanning_patterns",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
    ("secret_scanning_settings", Owned(&[Repository("repo_id")])),
    ("sessions", Owned(&[User("user_id")])),
    ("signature_verifications", Shared),
    ("signup_requests", Cell),
    ("ssh_keys", Owned(&[User("user_id")])),
    ("ssh_signing_keys", Owned(&[User("user_id")])),
    ("stars", Owned(&[Repository("repo_id")])),
    ("storage_object_copies", Cell),
    ("storage_objects", Shared),
    ("team_ancestors", Owned(&[Parent("team_id", "teams")])),
    ("team_invitations", Owned(&[Parent("team_id", "teams")])),
    ("team_memberships", Owned(&[Parent("team_id", "teams")])),
    ("team_repo_access", Owned(&[Repository("repo_id")])),
    ("teams", Owned(&[Organization("org_id")])),
    ("timeline_events", Owned(&[Repository("repo_id")])),
    ("topics", Shared),
    ("two_factor_auth", Owned(&[User("user_id")])),
    ("two_factor_recovery_codes", Owned(&[User("user_id")])),
    ("upload_rate_limit", Cell),
    ("user_blocks", Owned(&[User("user_id")])),
    ("user_emails", Owned(&[User("user_id")])),
    ("user_profile_settings", Owned(&[User("user_id")])),
    ("user_social_accounts", Owned(&[User("user_id")])),
    ("users", Owned(&[User("id")])),
    ("vulnerability_jobs", Owned(&[Repository("repo_id")])),
    ("vulnerability_settings", Cell),
    ("watches", Owned(&[Repository("repo_id")])),
    (
        "webhook_deliveries",
        Owned(&[Parent("webhook_id", "webhooks")]),
    ),
    (
        "webhooks",
        Owned(&[Repository("repo_id"), Organization("org_id")]),
    ),
];

pub fn rule(table: &str) -> Option<Rule> {
    TABLES
        .binary_search_by(|(name, _)| (*name).cmp(table))
        .ok()
        .map(|index| TABLES[index].1)
}

/// The column a parent reference points at; every parent is keyed by `id` except these.
pub fn parent_key(table: &str) -> &'static str {
    match table {
        "fine_grained_tokens" => "token_id",
        "github_app_hooks" => "app_id",
        _ => "id",
    }
}

/// The condition that selects the rows of `table` (aliased `t0`) belonging to the account
/// bound as `$1`, or `None` when the table's rows never move with an account. For
/// `ObjectKind::Repository`, `$1` is a repository and only rows that hang below it are
/// selected. Repository stubs are never selected: their rows live in another cell.
pub fn owned_rows(table: &str, account: ObjectKind) -> Option<String> {
    match rule(table)? {
        Owned(references) => Some(condition(references, account, 0)),
        Cell | Shared => None,
    }
}

fn condition(references: &[Reference], account: ObjectKind, depth: usize) -> String {
    let row = format!("t{depth}");
    let organization = account == ObjectKind::Organization;
    let repository = account == ObjectKind::Repository;
    let mut arms = String::from("CASE");
    for reference in references {
        let column = reference.column();
        let test = match *reference {
            User(_) | Organization(_) | Owner(..) if repository => "FALSE".to_owned(),
            User(column) if !organization => format!("{row}.{column}=$1"),
            Organization(column) if organization => format!("{row}.{column}=$1"),
            User(_) | Organization(_) => "FALSE".to_owned(),
            Owner(kind, column) => format!(
                "({row}.{kind}='{}' AND {row}.{column}=$1)",
                if organization { "org" } else { "user" }
            ),
            Repository(column) if repository => format!(
                "{row}.{column} IN (SELECT r{depth}.id FROM repositories r{depth} WHERE NOT r{depth}.is_stub AND r{depth}.id=$1)"
            ),
            Repository(column) => format!(
                "{row}.{column} IN (SELECT r{depth}.id FROM repositories r{depth} WHERE NOT r{depth}.is_stub AND r{depth}.{}=$1)",
                if organization { "owner_org_id" } else { "owner_user_id" }
            ),
            Parent(column, parent) => {
                let Some(Owned(parents)) = rule(parent) else {
                    return "FALSE".to_owned();
                };
                let next = depth + 1;
                format!(
                    "{row}.{column} IN (SELECT t{next}.{} FROM {parent} t{next} WHERE {})",
                    parent_key(parent),
                    condition(parents, account, next)
                )
            }
        };
        arms.push_str(&format!(" WHEN {row}.{column} IS NOT NULL THEN {test}"));
    }
    arms.push_str(" ELSE FALSE END");
    arms
}

/// The arguments of the table's `account_move_guard` trigger: one path per account a row can
/// reach. A path follows parent rows (`column@table.key>`) to an account column (`column:u`,
/// `column:o`, `column:k.kind_column`) or a repository column (`column:r`).
pub fn guard_paths(table: &str) -> Vec<String> {
    // A repository's own row does not exist before its insert, so its guard reads the owner,
    // and the row itself for a repository that moves on its own.
    if table == "repositories" {
        return vec![
            "owner_user_id:u".to_owned(),
            "owner_org_id:o".to_owned(),
            "id:r".to_owned(),
        ];
    }
    let mut paths = Vec::new();
    if let Some(Owned(references)) = rule(table) {
        collect_paths(references, "", &mut paths);
    }
    paths
}

fn collect_paths(references: &[Reference], prefix: &str, paths: &mut Vec<String>) {
    for reference in references {
        match *reference {
            User(column) => paths.push(format!("{prefix}{column}:u")),
            Organization(column) => paths.push(format!("{prefix}{column}:o")),
            Owner(kind, column) => paths.push(format!("{prefix}{column}:k.{kind}")),
            Repository(column) => paths.push(format!("{prefix}{column}:r")),
            Parent(column, parent) => {
                if let Some(Owned(parents)) = rule(parent) {
                    let prefix = format!("{prefix}{column}@{parent}.{}>", parent_key(parent));
                    collect_paths(parents, &prefix, paths);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const POSTGRES: &str = include_str!("../../../../schema/postgres/0001.schema.sql");

    /// Table name to its column definitions, from a baseline's `CREATE TABLE` statements.
    fn tables(schema: &str) -> BTreeMap<String, String> {
        let mut tables = BTreeMap::new();
        for statement in schema.split("CREATE TABLE ").skip(1) {
            let statement = statement.trim_start_matches("IF NOT EXISTS ");
            let name: String = statement
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let body = statement.split(";\n").next().unwrap_or_default();
            tables.insert(name, body.to_owned());
        }
        tables
    }

    fn has_column(body: &str, column: &str) -> bool {
        body.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|word| word == column)
    }

    /// Follows parents to an account or repository reference.
    fn reaches_owner(table: &str, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        match rule(table) {
            Some(Owned(references)) => references.iter().any(|reference| match reference {
                Parent(_, parent) => reaches_owner(parent, depth + 1),
                _ => true,
            }),
            _ => false,
        }
    }

    #[test]
    fn every_table_has_an_ownership_rule() {
        assert!(
            TABLES.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "keep TABLES sorted"
        );
        let postgres = tables(POSTGRES);
        let sqlite = tables(include_str!("../../../../schema/sqlite/0001.schema.sql"));
        assert!(postgres.len() > 250 && sqlite.len() > 250);
        for (engine, schema) in [("postgres", &postgres), ("sqlite", &sqlite)] {
            for (table, body) in schema.iter() {
                let rule = rule(table)
                    .unwrap_or_else(|| panic!("{engine} table {table} has no ownership rule"));
                if let Owned(references) = rule {
                    assert!(!references.is_empty(), "{table}");
                    for reference in references {
                        assert!(
                            has_column(body, reference.column()),
                            "{engine} {table} lacks {}",
                            reference.column()
                        );
                        match reference {
                            Parent(column, parent) => {
                                assert!(postgres.contains_key(*parent), "{table} names {parent}");
                                if engine == "postgres" {
                                    let key = parent_key(parent);
                                    assert!(
                                        body.contains(&format!("{parent}({key})")),
                                        "{table}.{column} does not reference {parent}({key})"
                                    );
                                }
                            }
                            Owner(kind, _) => assert!(has_column(body, kind), "{table} {kind}"),
                            _ => {}
                        }
                    }
                    assert!(reaches_owner(table, 0), "{table} never reaches an owner");
                }
            }
        }
        for (table, _) in TABLES {
            assert!(
                postgres.contains_key(*table) && sqlite.contains_key(*table),
                "{table} must be in both baseline catalogues for database copy"
            );
        }
    }

    /// Every owned PostgreSQL table carries the freeze guard with exactly its rule's paths,
    /// and no other table does.
    #[test]
    fn the_move_guard_covers_every_owned_table() {
        let mut guarded = BTreeMap::new();
        for statement in POSTGRES.split("CREATE TRIGGER ").skip(1) {
            let statement = statement.split(";\n").next().unwrap_or_default();
            let Some((_, arguments)) = statement.split_once("EXECUTE FUNCTION account_move_guard(")
            else {
                continue;
            };
            let table = statement
                .split(" ON ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_default()
                .to_owned();
            let paths: Vec<String> = arguments
                .trim_end_matches(')')
                .split(',')
                .map(|path| path.trim().trim_matches('\'').to_owned())
                .collect();
            assert!(
                guarded.insert(table.clone(), paths).is_none(),
                "{table} twice"
            );
        }
        for (table, rule) in TABLES {
            match rule {
                Owned(_) => assert_eq!(
                    guarded.remove(*table),
                    Some(guard_paths(table)),
                    "{table} move guard"
                ),
                Cell | Shared => assert!(!guarded.contains_key(*table), "{table} is not owned"),
            }
        }
        assert!(guarded.is_empty(), "guards on unknown tables: {guarded:?}");
    }

    #[test]
    fn owned_rows_follow_the_first_reference_and_the_account_kind() {
        let user = owned_rows("issue_comments", ObjectKind::User).unwrap();
        assert_eq!(
            user,
            "CASE WHEN t0.issue_id IS NOT NULL THEN t0.issue_id IN (SELECT t1.id FROM issues t1 WHERE CASE WHEN t1.repo_id IS NOT NULL THEN t1.repo_id IN (SELECT r1.id FROM repositories r1 WHERE NOT r1.is_stub AND r1.owner_user_id=$1) ELSE FALSE END) ELSE FALSE END"
        );
        let organization = owned_rows("webhooks", ObjectKind::Organization).unwrap();
        assert!(organization.contains("WHEN t0.org_id IS NOT NULL THEN t0.org_id=$1"));
        assert!(organization.contains("r0.owner_org_id=$1"));
        assert_eq!(
            owned_rows("repositories", ObjectKind::Repository).unwrap(),
            "CASE WHEN t0.id IS NOT NULL THEN t0.id IN (SELECT r0.id FROM repositories r0 WHERE NOT r0.is_stub AND r0.id=$1) ELSE FALSE END"
        );
        let repository = owned_rows("webhooks", ObjectKind::Repository).unwrap();
        assert!(repository.contains("r0.id=$1") && repository.contains("THEN FALSE"));
        assert_eq!(
            owned_rows("repository_redirects", ObjectKind::Repository).unwrap(),
            "CASE WHEN t0.owner_id IS NOT NULL THEN FALSE ELSE FALSE END"
        );
        assert!(owned_rows("nodes", ObjectKind::User).is_none());
        assert!(owned_rows("files", ObjectKind::User).is_none());
        assert_eq!(
            guard_paths("webhook_deliveries"),
            [
                "webhook_id@webhooks.id>repo_id:r",
                "webhook_id@webhooks.id>org_id:o"
            ]
        );
        assert_eq!(
            guard_paths("immutable_release_owner_names"),
            ["owner_id:k.owner_kind"]
        );
    }
}
