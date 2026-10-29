//! ProjectV2 compatibility selections used by `cli/cli`'s project commands.
//!
//! The complete generated SDL already registers the ProjectV2 objects, unions,
//! inputs, connections and mutations. Keep CLI compatibility fixtures here rather
//! than editing generated schema files or the GraphQL lane's resolver families.

/// `queries.ProjectMutationQuery`, including the zero-sized count connections.
pub const PROJECT_SELECTION: &str = r#"
fragment ProjectCLI on ProjectV2 {
  number url shortDescription public closed title id readme
  items(first: 0) { totalCount }
  fields(first: 0) { totalCount }
  owner { __typename ... on User { login } ... on Organization { login } }
}
"#;

/// `queries.ProjectFields` with every field kind understood by `gh project`.
pub const FIELD_SELECTION: &str = r#"
fragment FieldCLI on ProjectV2FieldConfiguration {
  __typename
  ... on ProjectV2Field { id name dataType }
  ... on ProjectV2IterationField { id name dataType }
  ... on ProjectV2SingleSelectField { id name dataType options { id name } }
}
"#;

/// `queries.ProjectItem`: linked content and typed values, not REST documents.
pub const ITEM_SELECTION: &str = r#"
fragment ItemCLI on ProjectV2Item {
  id
  content {
    __typename
    ... on DraftIssue { id title body }
    ... on Issue { title body number url repository { nameWithOwner } }
    ... on PullRequest { title body number url repository { nameWithOwner } }
  }
  fieldValues(first: 100) {
    nodes {
      __typename
      ... on ProjectV2ItemFieldDateValue { date field { ...FieldCLI } }
      ... on ProjectV2ItemFieldNumberValue { number field { ...FieldCLI } }
      ... on ProjectV2ItemFieldSingleSelectValue { name field { ...FieldCLI } }
      ... on ProjectV2ItemFieldTextValue { text field { ...FieldCLI } }
      ... on ProjectV2ItemFieldIterationValue { title startDate duration iterationId field { ...FieldCLI } }
      ... on ProjectV2ItemFieldLabelValue { labels(first: 10) { nodes { name } } field { ...FieldCLI } }
      ... on ProjectV2ItemFieldMilestoneValue { milestone { title description dueOn } field { ...FieldCLI } }
      ... on ProjectV2ItemFieldPullRequestValue { pullRequests(first: 10) { nodes { url } } field { ...FieldCLI } }
      ... on ProjectV2ItemFieldRepositoryValue { repository { url } field { ...FieldCLI } }
      ... on ProjectV2ItemFieldUserValue { users(first: 10) { nodes { login } } field { ...FieldCLI } }
      ... on ProjectV2ItemFieldReviewerValue { reviewers(first: 10) { nodes { __typename ... on Team { name } ... on User { login } } } field { ...FieldCLI } }
    }
  }
}
"#;
