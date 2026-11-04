//! Project bindings kept separate from the shared GraphQL lane.
use crate::bindings::abstract_types;
use crate::bindings::connection;
use crate::bindings::enums;
use crate::bindings::fields;
use crate::bindings::objects;
use crate::bindings::scalars;
use crate::bindings::Object;
use crate::bindings::Option;
use crate::bindings::Vec;
use async_graphql::ID;
use classic_fields;
use list_fields;
use mutation_fields;
use owner_fields;

scalars!(Date);
enums!(ProjectV2FieldType, ProjectV2ItemType);
objects!(
    Project,
    ProjectConnection,
    ProjectEdge,
    ProjectV2,
    ProjectV2Item,
    DraftIssue,
    ProjectV2Field,
    ProjectV2SingleSelectField,
    ProjectV2IterationField,
    ProjectV2SingleSelectFieldOption,
    ProjectV2IterationFieldConfiguration,
    ProjectV2IterationFieldIteration,
    ProjectV2Connection,
    ProjectV2Edge,
    ProjectV2FieldConfigurationConnection,
    ProjectV2FieldConfigurationEdge,
    ProjectV2ItemConnection,
    ProjectV2ItemEdge,
    ProjectV2ItemFieldValueConnection,
    ProjectV2ItemFieldValueEdge,
    ProjectV2ItemFieldTextValue,
    ProjectV2ItemFieldNumberValue,
    ProjectV2ItemFieldDateValue,
    ProjectV2ItemFieldSingleSelectValue,
    ProjectV2ItemFieldIterationValue,
    ProjectV2ItemFieldLabelValue,
    ProjectV2ItemFieldMilestoneValue,
    ProjectV2ItemFieldPullRequestValue,
    ProjectV2ItemFieldRepositoryValue,
    ProjectV2ItemFieldReviewerValue,
    ProjectV2ItemFieldUserValue,
    RequestedReviewerConnection,
    RequestedReviewerEdge,
    OrganizationConnection,
    OrganizationEdge,
    LinkProjectV2ToTeamPayload,
    UnlinkProjectV2FromTeamPayload,
    CreateProjectV2Payload,
    UpdateProjectV2Payload,
    DeleteProjectV2Payload,
    CopyProjectV2Payload,
    CreateProjectV2FieldPayload,
    DeleteProjectV2FieldPayload,
    AddProjectV2ItemByIdPayload,
    AddProjectV2DraftIssuePayload,
    UpdateProjectV2DraftIssuePayload,
    UpdateProjectV2ItemFieldValuePayload,
    ClearProjectV2ItemFieldValuePayload,
    ArchiveProjectV2ItemPayload,
    UnarchiveProjectV2ItemPayload,
    DeleteProjectV2ItemPayload,
    LinkProjectV2ToRepositoryPayload,
    UnlinkProjectV2FromRepositoryPayload,
    MarkProjectV2AsTemplatePayload,
    UnmarkProjectV2AsTemplatePayload,
    UpdateProjectV2ItemPositionPayload
);
abstract_types!(ProjectV2Owner: User, Organization);
abstract_types!(UniformResourceLocatable: Issue, PullRequest, Repository, User, Organization);
abstract_types!(ProjectV2ItemContent: Issue, PullRequest, DraftIssue);
abstract_types!(ProjectV2FieldConfiguration: ProjectV2Field, ProjectV2SingleSelectField, ProjectV2IterationField);
abstract_types!(ProjectV2ItemFieldValue: ProjectV2ItemFieldTextValue, ProjectV2ItemFieldNumberValue, ProjectV2ItemFieldDateValue, ProjectV2ItemFieldSingleSelectValue, ProjectV2ItemFieldIterationValue, ProjectV2ItemFieldLabelValue, ProjectV2ItemFieldMilestoneValue, ProjectV2ItemFieldPullRequestValue, ProjectV2ItemFieldRepositoryValue, ProjectV2ItemFieldReviewerValue, ProjectV2ItemFieldUserValue);

macro_rules! list_fields {
    ($ty:literal) => { fields!($ty;
        projects_v2(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, min_permission_level: Option<gql::ProjectV2PermissionLevel>, order_by: Option<gql::Input<gql::ProjectV2Order>>, query: Option<String>) -> gql::ProjectV2Connection;
    ); };
}

macro_rules! classic_fields {
    ($ty:literal) => { fields!($ty;
        projects(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::ProjectOrder>>, search: Option<String>, states: Option<Vec<gql::ProjectState>>) -> gql::ProjectConnection;
    ); };
}

macro_rules! owner_fields {
    ($ty:literal) => {
        projects::list_fields!($ty);
        fields!($ty;
            project_v2(number: i32) -> Option<gql::ProjectV2>;
            viewer_can_create_projects() -> bool;
        );
    };
}

macro_rules! mutation_fields {
    () => { fields!("Mutation";
        create_project_v2(input: gql::Input<gql::CreateProjectV2Input>) -> Option<gql::CreateProjectV2Payload>;
        update_project_v2(input: gql::Input<gql::UpdateProjectV2Input>) -> Option<gql::UpdateProjectV2Payload>;
        delete_project_v2(input: gql::Input<gql::DeleteProjectV2Input>) -> Option<gql::DeleteProjectV2Payload>;
        copy_project_v2(input: gql::Input<gql::CopyProjectV2Input>) -> Option<gql::CopyProjectV2Payload>;
        create_project_v2_field(input: gql::Input<gql::CreateProjectV2FieldInput>) -> Option<gql::CreateProjectV2FieldPayload>;
        delete_project_v2_field(input: gql::Input<gql::DeleteProjectV2FieldInput>) -> Option<gql::DeleteProjectV2FieldPayload>;
        add_project_v2_item_by_id(input: gql::Input<gql::AddProjectV2ItemByIdInput>) -> Option<gql::AddProjectV2ItemByIdPayload>;
        add_project_v2_draft_issue(input: gql::Input<gql::AddProjectV2DraftIssueInput>) -> Option<gql::AddProjectV2DraftIssuePayload>;
        update_project_v2_draft_issue(input: gql::Input<gql::UpdateProjectV2DraftIssueInput>) -> Option<gql::UpdateProjectV2DraftIssuePayload>;
        update_project_v2_item_field_value(input: gql::Input<gql::UpdateProjectV2ItemFieldValueInput>) -> Option<gql::UpdateProjectV2ItemFieldValuePayload>;
        clear_project_v2_item_field_value(input: gql::Input<gql::ClearProjectV2ItemFieldValueInput>) -> Option<gql::ClearProjectV2ItemFieldValuePayload>;
        archive_project_v2_item(input: gql::Input<gql::ArchiveProjectV2ItemInput>) -> Option<gql::ArchiveProjectV2ItemPayload>;
        unarchive_project_v2_item(input: gql::Input<gql::UnarchiveProjectV2ItemInput>) -> Option<gql::UnarchiveProjectV2ItemPayload>;
        delete_project_v2_item(input: gql::Input<gql::DeleteProjectV2ItemInput>) -> Option<gql::DeleteProjectV2ItemPayload>;
        link_project_v2_to_team(input: gql::Input<gql::LinkProjectV2ToTeamInput>) -> Option<gql::LinkProjectV2ToTeamPayload>;
        unlink_project_v2_from_team(input: gql::Input<gql::UnlinkProjectV2FromTeamInput>) -> Option<gql::UnlinkProjectV2FromTeamPayload>;
        link_project_v2_to_repository(input: gql::Input<gql::LinkProjectV2ToRepositoryInput>) -> Option<gql::LinkProjectV2ToRepositoryPayload>;
        unlink_project_v2_from_repository(input: gql::Input<gql::UnlinkProjectV2FromRepositoryInput>) -> Option<gql::UnlinkProjectV2FromRepositoryPayload>;
        mark_project_v2_as_template(input: gql::Input<gql::MarkProjectV2AsTemplateInput>) -> Option<gql::MarkProjectV2AsTemplatePayload>;
        unmark_project_v2_as_template(input: gql::Input<gql::UnmarkProjectV2AsTemplateInput>) -> Option<gql::UnmarkProjectV2AsTemplatePayload>;
        update_project_v2_item_position(input: gql::Input<gql::UpdateProjectV2ItemPositionInput>) -> Option<gql::UpdateProjectV2ItemPositionPayload>;
    ); };
}

impl gql::ProjectResolver for Object {
    fields!("Project"; id() -> ID; name() -> String; number() -> i32; resource_path() -> gql::URI;);
}
connection!(
    ProjectConnectionResolver,
    ProjectEdgeResolver,
    "ProjectConnection",
    "ProjectEdge",
    Project,
    ProjectEdge
);

impl gql::ProjectV2Resolver for Object {
    fields!("ProjectV2";
        id() -> ID; database_id() -> Option<i32>; number() -> i32; title() -> String;
        short_description() -> Option<String>; readme() -> Option<String>; public() -> bool; closed() -> bool; template() -> bool;
        url() -> gql::URI; resource_path() -> gql::URI; created_at() -> gql::DateTime; updated_at() -> gql::DateTime; closed_at() -> Option<gql::DateTime>;
        owner() -> gql::ProjectV2Owner; creator() -> Option<gql::Actor>;
        viewer_can_update() -> bool; viewer_can_close() -> bool; viewer_can_reopen() -> bool;
        field(name: String) -> Option<gql::ProjectV2FieldConfiguration>;
        fields(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::ProjectV2FieldOrder>>) -> gql::ProjectV2FieldConfigurationConnection;
        items(after: Option<String>, archived_states: Option<Vec<gql::ProjectV2ItemArchivedState>>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::ProjectV2ItemOrder>>, query: Option<String>) -> gql::ProjectV2ItemConnection;
    );
}
impl gql::ProjectV2ItemResolver for Object {
    fields!("ProjectV2Item";
        id() -> ID; database_id() -> Option<i32>; content() -> Option<gql::ProjectV2ItemContent>; project() -> gql::ProjectV2; is_archived() -> bool;
        created_at() -> gql::DateTime; updated_at() -> gql::DateTime; creator() -> Option<gql::Actor>;
        field_value_by_name(name: String) -> Option<gql::ProjectV2ItemFieldValue>;
        field_values(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::ProjectV2ItemFieldValueOrder>>) -> gql::ProjectV2ItemFieldValueConnection;
    );
}
impl gql::DraftIssueResolver for Object {
    fields!("DraftIssue"; id() -> ID; title() -> String; body() -> String; created_at() -> gql::DateTime; updated_at() -> gql::DateTime; creator() -> Option<gql::Actor>;);
}
macro_rules! field_properties {
    ($ty:literal) => { fields!($ty;
        id() -> ID; database_id() -> Option<i32>; name() -> String; data_type() -> gql::ProjectV2FieldType; is_issue_field() -> bool;
        created_at() -> gql::DateTime; updated_at() -> gql::DateTime; project() -> gql::ProjectV2;
    ); };
}
impl gql::ProjectV2FieldResolver for Object {
    field_properties!("ProjectV2Field");
}
impl gql::ProjectV2SingleSelectFieldResolver for Object {
    field_properties!("ProjectV2SingleSelectField");
    fields!("ProjectV2SingleSelectField"; options(names: Option<Vec<String>>) -> Vec<gql::ProjectV2SingleSelectFieldOption>;);
}
impl gql::ProjectV2IterationFieldResolver for Object {
    field_properties!("ProjectV2IterationField");
    fields!("ProjectV2IterationField"; configuration() -> gql::ProjectV2IterationFieldConfiguration;);
}
impl gql::ProjectV2SingleSelectFieldOptionResolver for Object {
    fields!("ProjectV2SingleSelectFieldOption"; id() -> String; name() -> String; description() -> String;);
}
impl gql::ProjectV2IterationFieldConfigurationResolver for Object {
    fields!("ProjectV2IterationFieldConfiguration";
        duration() -> i32; start_day() -> i32; iterations() -> Vec<gql::ProjectV2IterationFieldIteration>; completed_iterations() -> Vec<gql::ProjectV2IterationFieldIteration>;
    );
}
impl gql::ProjectV2IterationFieldIterationResolver for Object {
    fields!("ProjectV2IterationFieldIteration"; id() -> String; title() -> String; start_date() -> gql::Date; duration() -> i32;);
}
macro_rules! value_field {
    ($ty:literal) => { fields!($ty; field() -> gql::ProjectV2FieldConfiguration;); };
}
impl gql::ProjectV2ItemFieldTextValueResolver for Object {
    value_field!("ProjectV2ItemFieldTextValue");
    fields!("ProjectV2ItemFieldTextValue"; text() -> Option<String>;);
}
impl gql::ProjectV2ItemFieldNumberValueResolver for Object {
    value_field!("ProjectV2ItemFieldNumberValue");
    fields!("ProjectV2ItemFieldNumberValue"; number() -> Option<f64>;);
}
impl gql::ProjectV2ItemFieldDateValueResolver for Object {
    value_field!("ProjectV2ItemFieldDateValue");
    fields!("ProjectV2ItemFieldDateValue"; date() -> Option<gql::Date>;);
}
impl gql::ProjectV2ItemFieldSingleSelectValueResolver for Object {
    value_field!("ProjectV2ItemFieldSingleSelectValue");
    fields!("ProjectV2ItemFieldSingleSelectValue"; name() -> Option<String>; option_id() -> Option<String>;);
}
impl gql::ProjectV2ItemFieldIterationValueResolver for Object {
    value_field!("ProjectV2ItemFieldIterationValue");
    fields!("ProjectV2ItemFieldIterationValue"; title() -> String; start_date() -> gql::Date; duration() -> i32; iteration_id() -> String;);
}
impl gql::ProjectV2ItemFieldLabelValueResolver for Object {
    value_field!("ProjectV2ItemFieldLabelValue");
    fields!("ProjectV2ItemFieldLabelValue"; labels(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> Option<gql::LabelConnection>;);
}
impl gql::ProjectV2ItemFieldMilestoneValueResolver for Object {
    value_field!("ProjectV2ItemFieldMilestoneValue");
    fields!("ProjectV2ItemFieldMilestoneValue"; milestone() -> Option<gql::Milestone>;);
}
impl gql::ProjectV2ItemFieldPullRequestValueResolver for Object {
    value_field!("ProjectV2ItemFieldPullRequestValue");
    fields!("ProjectV2ItemFieldPullRequestValue"; pull_requests(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::PullRequestOrder>>) -> Option<gql::PullRequestConnection>;);
}
impl gql::ProjectV2ItemFieldRepositoryValueResolver for Object {
    value_field!("ProjectV2ItemFieldRepositoryValue");
    fields!("ProjectV2ItemFieldRepositoryValue"; repository() -> Option<gql::Repository>;);
}
impl gql::ProjectV2ItemFieldReviewerValueResolver for Object {
    value_field!("ProjectV2ItemFieldReviewerValue");
    fields!("ProjectV2ItemFieldReviewerValue"; reviewers(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> Option<gql::RequestedReviewerConnection>;);
}
impl gql::ProjectV2ItemFieldUserValueResolver for Object {
    value_field!("ProjectV2ItemFieldUserValue");
    fields!("ProjectV2ItemFieldUserValue"; users(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> Option<gql::UserConnection>;);
}

connection!(
    ProjectV2ConnectionResolver,
    ProjectV2EdgeResolver,
    "ProjectV2Connection",
    "ProjectV2Edge",
    ProjectV2,
    ProjectV2Edge
);
connection!(
    ProjectV2FieldConfigurationConnectionResolver,
    ProjectV2FieldConfigurationEdgeResolver,
    "ProjectV2FieldConfigurationConnection",
    "ProjectV2FieldConfigurationEdge",
    ProjectV2FieldConfiguration,
    ProjectV2FieldConfigurationEdge
);
connection!(
    ProjectV2ItemConnectionResolver,
    ProjectV2ItemEdgeResolver,
    "ProjectV2ItemConnection",
    "ProjectV2ItemEdge",
    ProjectV2Item,
    ProjectV2ItemEdge
);
connection!(
    RequestedReviewerConnectionResolver,
    RequestedReviewerEdgeResolver,
    "RequestedReviewerConnection",
    "RequestedReviewerEdge",
    RequestedReviewer,
    RequestedReviewerEdge
);
connection!(
    ProjectV2ItemFieldValueConnectionResolver,
    ProjectV2ItemFieldValueEdgeResolver,
    "ProjectV2ItemFieldValueConnection",
    "ProjectV2ItemFieldValueEdge",
    ProjectV2ItemFieldValue,
    ProjectV2ItemFieldValueEdge
);
connection!(
    OrganizationConnectionResolver,
    OrganizationEdgeResolver,
    "OrganizationConnection",
    "OrganizationEdge",
    Organization,
    OrganizationEdge
);

macro_rules! payload {
    ($resolver:ident, $ty:literal; $($method:ident: $out:ty),*) => { impl gql::$resolver for Object { fields!($ty; client_mutation_id() -> Option<String>; $($method() -> $out;)*); } };
}
payload!(LinkProjectV2ToTeamPayloadResolver,"LinkProjectV2ToTeamPayload";);
payload!(UnlinkProjectV2FromTeamPayloadResolver,"UnlinkProjectV2FromTeamPayload";);
payload!(CreateProjectV2PayloadResolver,"CreateProjectV2Payload"; project_v2: Option<gql::ProjectV2>);
payload!(UpdateProjectV2PayloadResolver,"UpdateProjectV2Payload"; project_v2: Option<gql::ProjectV2>);
payload!(DeleteProjectV2PayloadResolver,"DeleteProjectV2Payload"; project_v2: Option<gql::ProjectV2>);
payload!(CopyProjectV2PayloadResolver,"CopyProjectV2Payload"; project_v2: Option<gql::ProjectV2>);
payload!(MarkProjectV2AsTemplatePayloadResolver,"MarkProjectV2AsTemplatePayload"; project_v2: Option<gql::ProjectV2>);
payload!(UnmarkProjectV2AsTemplatePayloadResolver,"UnmarkProjectV2AsTemplatePayload"; project_v2: Option<gql::ProjectV2>);
payload!(CreateProjectV2FieldPayloadResolver,"CreateProjectV2FieldPayload"; project_v2_field: Option<gql::ProjectV2FieldConfiguration>);
payload!(DeleteProjectV2FieldPayloadResolver,"DeleteProjectV2FieldPayload"; project_v2_field: Option<gql::ProjectV2FieldConfiguration>);
payload!(AddProjectV2ItemByIdPayloadResolver,"AddProjectV2ItemByIdPayload"; item: Option<gql::ProjectV2Item>);
payload!(AddProjectV2DraftIssuePayloadResolver,"AddProjectV2DraftIssuePayload"; project_item: Option<gql::ProjectV2Item>);
payload!(UpdateProjectV2DraftIssuePayloadResolver,"UpdateProjectV2DraftIssuePayload"; draft_issue: Option<gql::DraftIssue>);
payload!(UpdateProjectV2ItemFieldValuePayloadResolver,"UpdateProjectV2ItemFieldValuePayload"; project_v2_item: Option<gql::ProjectV2Item>);
payload!(ClearProjectV2ItemFieldValuePayloadResolver,"ClearProjectV2ItemFieldValuePayload"; project_v2_item: Option<gql::ProjectV2Item>);
payload!(ArchiveProjectV2ItemPayloadResolver,"ArchiveProjectV2ItemPayload"; item: Option<gql::ProjectV2Item>);
payload!(UnarchiveProjectV2ItemPayloadResolver,"UnarchiveProjectV2ItemPayload"; item: Option<gql::ProjectV2Item>);
payload!(DeleteProjectV2ItemPayloadResolver,"DeleteProjectV2ItemPayload"; deleted_item_id: Option<ID>);
payload!(LinkProjectV2ToRepositoryPayloadResolver,"LinkProjectV2ToRepositoryPayload"; repository: Option<gql::Repository>);
payload!(UnlinkProjectV2FromRepositoryPayloadResolver,"UnlinkProjectV2FromRepositoryPayload"; repository: Option<gql::Repository>);
impl gql::UpdateProjectV2ItemPositionPayloadResolver for Object {
    fields!("UpdateProjectV2ItemPositionPayload";
        client_mutation_id() -> Option<String>;
        items(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> Option<gql::ProjectV2ItemConnection>;
    );
}
