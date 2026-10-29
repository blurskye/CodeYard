# GitHub CLI GraphQL coverage

This table covers the GraphQL requests made by gh 2.102.0, not end-to-end Git,
REST, browser, or local configuration behavior. **Works** means the exact query,
including fragments, passes through the real `/api/graphql` router on SQLite and
PostgreSQL in `tests/repositories/graphql/gh_cli.rs`. Every selected response key
is checked, with additional assertions for stored state, populated comments,
reviews, assignees, statuses, permissions, and pagination. **Fails** also includes
requests without that proof; it does not necessarily mean the command is broken.
Arbitrary `--json` selections and flags not listed below are not certified.

The fields column summarizes selections; each linked fixture contains the full,
unmodified query and nested fields. Captured queries come from the installed CLI;
`pr-create`, `pr-search`, `pr-status-number`, `repo-template`, and `repo-network` use the literal
upstream templates with their normal field/alias substitutions. `paginate` is a
user-supplied query, which gh forwards unchanged.

Pull-request, repository lifecycle and pagination tables: **34 works / 0 fails**.
Release, organization and repository-list tables: **17 works / 0 fails**.
Issue and label table: **47 works / 0 fails**.
Status/search, Discussions, project and project-metadata tables: **72 works / 0 fails**.
GraphQL-only tables combined: **170 works / 0 fails**.

## gh pr

The list selection is `number,title,state,url,headRefName,headRepositoryOwner
{id,login,...User{name}},isCrossRepository,isDraft,createdAt`. Finder operations
also select `id`. Feature-detection queries `PullRequest_fields` and
`PullRequest_fields2` select `__type.fields.name` and are tested too.

| Command | Operation name | Fields selected | Status |
|---|---|---|---|
| `pr list` | PullRequestList | [List selection](../tests/repositories/graphql/cli/pr-list.graphql), totalCount, pageInfo {hasNextPage,endCursor} | works |
| `pr list --search` | PullRequestSearch | [List selection](../tests/repositories/graphql/cli/pr-search.graphql), issueCount, pageInfo {hasNextPage,endCursor} | works |
| `pr view` | PullRequestByNumber; PullRequestProjectItems | [url,number,title,state,body,author,autoMergeRequest,isDraft,maintainerCanModify,mergeable,additions,deletions,commits.totalCount,baseRefName,headRefName,headRepositoryOwner,headRepository,isCrossRepository,reviewRequests,reviews,assignees,labels,milestone,comments,reactionGroups,createdAt,statusCheckRollup,id](../tests/repositories/graphql/cli/pr-view.graphql); [projectItems {totalCount,nodes {id,project {id,title},status {optionId,name}},pageInfo}](../tests/repositories/graphql/cli/pr-project-items.graphql) | works |
| `pr create` | RepositoryInfo; PullRequestCreate | [Repository metadata and parent](../tests/repositories/graphql/cli/repo-info.graphql); [pullRequest {id,url}](../tests/repositories/graphql/cli/pr-create.graphql) | works |
| `pr merge --merge` | PullRequestByNumber; PullRequestMerge | [id,number,state,title,commits(last:1).commit.oid,mergeStateStatus,headRefName,baseRefName,headRefOid,isCrossRepository,isInMergeQueue,isMergeQueueEnabled](../tests/repositories/graphql/cli/pr-merge-find.graphql); [clientMutationId](../tests/repositories/graphql/cli/pr-merge.graphql) | works |
| `pr checkout` | PullRequestByNumber | [number,headRefName,headRepository {id,name,nameWithOwner},headRepositoryOwner {id,login,name},isCrossRepository,maintainerCanModify,id](../tests/repositories/graphql/cli/pr-checkout.graphql) | works |
| `pr status` | UserCurrent; PullRequestStatus | [viewer.login](../tests/repositories/graphql/cli/viewer.graphql); [PR identity,head refs,mergeStateStatus,baseRef.branchProtectionRule.requiresStrictStatusChecks,autoMergeRequest,status rollup counts,reviewDecision,latestReviews; defaultBranchRef,viewerCreated,reviewRequested](../tests/repositories/graphql/cli/pr-status.graphql); [numbered-current-PR variant adds requiredApprovingReviewCount](../tests/repositories/graphql/cli/pr-status-number.graphql) | works |
| `pr checks` | PullRequestByNumber; PullRequestStatusChecks | [number,id](../tests/repositories/graphql/cli/pr-checks-find.graphql); [commits(last:1).commit.statusCheckRollup.contexts {StatusContext {context,state,targetUrl,createdAt,description,isRequired},CheckRun {name,checkSuite.workflowRun {event,workflow.name},status,conclusion,startedAt,completedAt,detailsUrl,isRequired},pageInfo}](../tests/repositories/graphql/cli/pr-checks.graphql) | works |
| `pr diff` | PullRequestByNumber | [number,id](../tests/repositories/graphql/cli/pr-diff.graphql); diff bytes use REST | works |
| `pr review --comment` | PullRequestByNumber; PullRequestReviewAdd | [id,number](../tests/repositories/graphql/cli/pr-review-find.graphql); [clientMutationId](../tests/repositories/graphql/cli/pr-review.graphql) | works |
| `pr comment --body` | PullRequestByNumber; CommentCreate | [id,number](../tests/repositories/graphql/cli/pr-comment-find.graphql); [commentEdge.node.url](../tests/repositories/graphql/cli/pr-comment.graphql) | works |
| `pr edit --title` | PullRequestByNumber; PullRequestProjectItems; PullRequestUpdate | [id,author,url,title,body,baseRefName,reviewRequests,labels,milestone,assignees](../tests/repositories/graphql/cli/pr-edit-find.graphql); projectItems; [__typename](../tests/repositories/graphql/cli/pr-edit.graphql) | works |
| `pr close` | PullRequestByNumber; PullRequestClose | [state,number,title,isCrossRepository,headRefName,id](../tests/repositories/graphql/cli/pr-close-find.graphql); [pullRequest.id](../tests/repositories/graphql/cli/pr-close.graphql) | works |
| `pr reopen` | PullRequestByNumber; PullRequestReopen | [id,number,state,title](../tests/repositories/graphql/cli/pr-reopen-find.graphql); [pullRequest.id](../tests/repositories/graphql/cli/pr-reopen.graphql) | works |
| `pr ready` | PullRequestByNumber; PullRequestReadyForReview | [id,number,state,isDraft](../tests/repositories/graphql/cli/pr-ready-find.graphql); [pullRequest.id](../tests/repositories/graphql/cli/pr-ready.graphql) | works |
| `pr lock --reason spam` | IssueByNumber; LockLockable | [hasIssuesEnabled; issueOrPullRequest {__typename,activeLockReason,id,locked,number,title,url}](../tests/repositories/graphql/cli/pr-lock-find.graphql); [lockedRecord.locked](../tests/repositories/graphql/cli/pr-lock.graphql) | works |
| `pr unlock` | IssueByNumber; UnlockLockable | Same finder; [unlockedRecord.locked](../tests/repositories/graphql/cli/pr-unlock.graphql) | works |
| `pr update-branch` | PullRequestByNumber; ComparePullRequestBaseBranchWith; PullRequestUpdateBranch | [id,number,headRefName,headRefOid,headRepositoryOwner,mergeable](../tests/repositories/graphql/cli/pr-update-find.graphql); [baseRef.compare {aheadBy,behindBy,status}](../tests/repositories/graphql/cli/pr-compare.graphql); [pullRequest.id](../tests/repositories/graphql/cli/pr-update.graphql) | works |
| `pr revert` | PullRequestRevert | [pullRequest.id,revertPullRequest {id,number,url}](../tests/repositories/graphql/cli/pr-revert.graphql) | works |
| `pr ready --undo` | ConvertPullRequestToDraft | [pullRequest.id](../tests/repositories/graphql/cli/pr-draft.graphql) | works |
| `pr merge --auto` / `--disable-auto` | PullRequestAutoMerge; PullRequestAutoMergeDisable | [clientMutationId](../tests/repositories/graphql/cli/pr-merge-auto.graphql); [clientMutationId](../tests/repositories/graphql/cli/pr-merge-auto-disable.graphql) | works: the request is stored and a background check merges the pull request through the normal merge path once it is mergeable |
| Interactive merge message lookup | PullRequestMergeText | [viewerMergeHeadlineText,viewerMergeBodyText](../tests/repositories/graphql/cli/pr-merge-text.graphql) | works |
| `pr merge --author-email` | PullRequestMerge | clientMutationId | works |

`pr checks` is proven with a populated StatusContext, not an Actions CheckRun.
The query includes both union fragments unchanged; Actions-backed contexts still
need their owning lane. Same-repository branch updates are tested; renamed forks,
rebase updates, and subsequent comment/review pages are not certified here.

## gh repo

The shared [RepositoryInfo](../tests/repositories/graphql/cli/repo-info.graphql)
selection is `id,databaseId,name,owner.login,sshUrl,hasIssuesEnabled,description,
hasWikiEnabled,viewerPermission,defaultBranchRef.name`, repeated for `parent`,
plus `mergeCommitAllowed,rebaseMergeAllowed,squashMergeAllowed`.

| Command | Operation name | Fields selected | Status |
|---|---|---|---|
| `repo view` | RepositoryInfo | [name,owner {id,login},description](../tests/repositories/graphql/cli/repo-view.graphql) | works |
| `repo clone` | RepositoryInfo | Shared RepositoryInfo selection | works |
| `repo create --disable-wiki` | RepositoryCreate | [repository {id,name,owner.login,url}](../tests/repositories/graphql/cli/repo-create.graphql) | works |
| `repo create` with default flags | RepositoryCreate | Same selection; input enables wiki | works |
| `repo create --template` | CloneTemplateRepository | [repository {id,name,owner.login,url}](../tests/repositories/graphql/cli/repo-template.graphql) | works |
| `repo delete` | None (REST `DELETE /repos/{owner}/{repo}`) | None | works |
| `repo sync` | RepositoryInfo | Shared RepositoryInfo selection; synchronization uses REST/Git | works |
| `repo rename` | None (REST `PATCH /repos/{owner}/{repo}` with `name`) | None | works |
| `repo archive` | RepositoryInfo; ArchiveRepository | [name,owner {id,login},isArchived,id](../tests/repositories/graphql/cli/repo-archive-find.graphql); [repository.id](../tests/repositories/graphql/cli/repo-archive.graphql) | works |
| `repo set-default` | RepositoryNetwork | [viewer.login; repo_000 {id,databaseId,name,owner.login,viewerPermission,defaultBranchRef.name,isPrivate,parent {same fields}}](../tests/repositories/graphql/cli/repo-network.graphql) | works |

Wiki enablement and template status are saved under native repository administration
permissions. Wiki content hosting is not implemented. Template clones copy current
branch trees into independent root commits, without source history, tags or fork linkage;
`includeAllBranches` and private-source refusals are tested. Revert tests open and merge
a draft revert, compare its Git tree, and refuse unmerged or already-reverted changes.

## gh api graphql

| Command | Operation name | Fields selected | Status |
|---|---|---|---|
| `api graphql --paginate -F query=@tests/repositories/graphql/cli/paginate.graphql` | Unnamed user query | [repository.pullRequests {nodes {number,title},pageInfo {hasNextPage,endCursor}}](../tests/repositories/graphql/cli/paginate.graphql) | works |

The pagination test follows `$endCursor` across two one-item pages and checks
termination, unique results, and changing non-null cursors. This proves the
GraphQL cursor contract, not execution of a separate gh process against Codeyard.

## gh release, gh org, gh repo list, gh repo fork, gh repo edit

Traced against gh 2.102.0 in `api/queries_{repo,user}.go`, `api/query_builder.go`,
`context/context.go`, `internal/featuredetection/feature_detection.go`, and the command
packages under `pkg/cmd/{release,org,repo/{list,fork,edit}}`. Query templates generated
from Go structs use githubv4's variable ordering and field aliases. The exact emitted
texts and string-query templates are exercised in
`tests/repositories/graphql/gh_teams.rs` on both database engines.

Coverage: **17 works / 0 fails**. The query contracts are
`sqlite_gh_teams_queries`, `postgres_gh_teams_queries`,
`sqlite_gh_repo_list_metadata` and `postgres_gh_repo_list_metadata`.

These commands send **no GraphQL mutations**. Release writes, repository forks and
repository edits use REST. `release verify` and `release verify-asset` use the
attestations API, not GraphQL. `release view/download` without a tag use REST's latest
release; tagged view/download/edit/delete/upload/delete-asset can also look up a draft
through `RepositoryReleaseByTag`. `release list --json` changes the exported data, not
the GraphQL selection. Org currently has only the `list` subcommand.

`D` below means the default repository fields:
`nameWithOwner,description,isPrivate,isFork,isArchived,createdAt,pushedAt,visibility`.
Servers without the visibility feature omit `visibility`.
`P` means `pageInfo{hasNextPage,endCursor}`.

| Command | Operation name | Selected fields | Status |
| --- | --- | --- | --- |
| release list | `Release_fields` | `Release:__type(name:"Release"){fields{name}}` | works |
| release list, including all `--json` choices | `RepositoryReleaseList` | `repository.releases(first,after,orderBy:CREATED_AT){nodes{name,tagName,isDraft,immutable,isLatest,isPrerelease,createdAt,publishedAt},P}` | works |
| release list on servers without immutable releases | `RepositoryReleaseList` | Same connection, omitting `immutable` | works |
| release create `--verify-tag`, release edit `--verify-tag` | `RepositoryFindRef` | `repository.ref(qualifiedName:$tagName){id}` | works |
| tagged release view/download/edit/delete/upload/delete-asset | `RepositoryReleaseByTag` | `repository.release(tagName:$tagName){databaseId,isDraft}` | works |
| org list | `UserCurrent` | `viewer{login}` | works |
| org list | `OrganizationList` | `user(login:$user){login,organizations(first,after){totalCount,nodes{login},P}}` | works |
| repo list; interactive repo edit | `Repository_fields` | `Repository:__type(name:"Repository"){fields(includeDeprecated:true){name}}` | works |
| repo list without owner | `RepositoryList` | `repositoryOwner:viewer{login,repositories(first,after,privacy,isFork,ownerAffiliations:OWNER,orderBy:PUSHED_AT_DESC){nodes{D},totalCount,P}}` | works |
| repo list OWNER, including organizations | `RepositoryList` | `repositoryOwner(login:$owner){login,repositories(first,after,privacy,isFork,ownerAffiliations:OWNER,orderBy:PUSHED_AT_DESC){nodes{D},totalCount,P}}` | works |
| repo list `--archived`, `--no-archived`, `--language`, `--topic`, or `--visibility internal` | `RepositoryListSearch` | `search(type:REPOSITORY,query,first,after){repositoryCount,nodes{...on Repository{D}},P}` | works |
| repo list `--json` (supported selections below), with or without owner | `RepositoryList` | Same owner connection; `nodes{J}` instead of `nodes{D}` | works |
| repo list search flags plus `--json` (supported selections below) | `RepositoryListSearch` | Same search connection; `nodes{...on Repository{J}}` | works |
| interactive repo edit | `RepositoryInfo` | `repository{defaultBranchRef{name},deleteBranchOnMerge,description,hasIssuesEnabled,hasProjectsEnabled,hasWikiEnabled,homepageUrl,isInOrganization,isTemplate,mergeCommitAllowed,rebaseMergeAllowed,repositoryTopics(first:100){nodes{topic{name}}},stargazerCount,squashMergeAllowed,watchers{totalCount},visibility,autoMergeAllowed}`; feature detection can omit either or both of the last two fields | works |
| repo edit security/analysis flags | `RepositoryInfo` | `repository{viewerCanAdminister}` | works |
| release/repo fork/repo edit when resolving local Git remotes | `RepositoryNetwork` | `viewer{login}`, aliased `repo_NNN:repository{...repo,parent{...repo}}`; `fragment repo on Repository{id,databaseId,name,owner{login},viewerPermission,defaultBranchRef{name},isPrivate}` | works |
| repo list `--json` with the additional selections below | `RepositoryList` / `RepositoryListSearch` | Same templates, using the additional selections below | works |

`J` selections currently answered from the existing stores (including nested selections
chosen by `RepositoryGraphQL`):

- Scalars: `id,name,nameWithOwner,description,homepageUrl,url,sshUrl,createdAt,pushedAt,updatedAt,
  hasIssuesEnabled,hasProjectsEnabled,hasWikiEnabled,mergeCommitAllowed,squashMergeAllowed,
  rebaseMergeAllowed,forkCount,stargazerCount,deleteBranchOnMerge,isArchived,isFork,
  isInOrganization,isPrivate,visibility,isTemplate,viewerCanAdminister,viewerPermission`.
- `owner{id,login}`, `parent{id,name,owner{id,login}}`, `defaultBranchRef{name}`,
  `repositoryTopics(first:100){nodes{topic{name}}}`, `watchers{totalCount}`,
  `labels(first:100){nodes{id,color,name,description}}`,
  `latestRelease{publishedAt,tagName,name,url}`.

Additional `RepositoryGraphQL` selections, all available through `repo list --json`:

- Scalars: `openGraphImageUrl,usesCustomOpenGraphImage,mirrorUrl,securityPolicyUrl,archivedAt,
  isBlankIssuesEnabled,isSecurityPolicyEnabled,hasDiscussionsEnabled,diskUsage,isEmpty,isMirror,
  isUserConfigurationRepository,viewerDefaultCommitEmail,viewerDefaultMergeMethod,
  viewerHasStarred,viewerPossibleCommitEmails,viewerSubscription`.
- `templateRepository{id,name,owner{id,login}}`, `codeOfConduct{key,name,url}`,
  `contactLinks{about,name,url}`, `fundingLinks{platform,url}`, `licenseInfo{key,name,nickname}`,
  `languages(first:100){edges{size,node{name}}}`, `primaryLanguage{name}`,
  `issueTemplates{name,title,body,about}`, `pullRequestTemplates{body,filename}`,
  `milestones(first:100,states:OPEN){nodes{number,title,description,dueOn}}`,
  `assignableUsers(first:100){nodes{id,login,name}}`,
  `mentionableUsers(first:100){nodes{id,login,name}}`,
  `projects(first:100,states:OPEN){nodes{id,name,number,body,resourcePath}}`,
  `projectsV2(first:100,query:"is:open"){nodes{id,number,title,resourcePath,closed,url}}`.
- `issues(states:OPEN){totalCount}` and `pullRequests(states:OPEN){totalCount}` work
  without `first` or `last`; selections of nodes, edges or pageInfo still require a page.

Git-backed fields read the default branch through RepoStore, including issue configuration,
security policy, templates, license, conduct and funding files. Languages use the existing
byte inventory. Projects v2 lists readable linked projects; classic projects are empty because
Codeyard has no classic projects. Mirrors, custom social images and template repositories are
not configured. `archivedAt` is null because archive times are not recorded; it does not invent
an archive timestamp from the repository's last metadata edit.

Explicit `repo fork OWNER/REPO` and non-interactive `repo edit` without security/analysis
flags send no GraphQL at all. Their REST behavior is not verified by these tests.

## gh issue, gh label

Traced against CLI 2.102.0 in `api/queries_{issue,comments,repo,projects_v2,branch_issue_reference,user}.go`,
`api/query_builder.go`, `internal/featuredetection/feature_detection.go`, `pkg/cmd/issue/`,
`pkg/cmd/label/`, and `pkg/cmd/issue/shared/lookup.go`, and `pkg/cmd/pr/shared/{editable_http,templates}.go`.
Codeyard is an enterprise hostname: actor APIs (`suggestedActors`,
`replaceActorsForAssignable`) are github.com-only and are not sent here.
`--web` generally opens a URL; label create/edit/delete use REST, not GraphQL.

Each row is an operation family; repeated pagination requests and flag-selected
scalar subsets share a row. Where flags introduce unsupported fields or inputs,
they have a separate failing row. “Works” requires the exact CLI document through
the router on both database engines, not just schema introspection. The issue contracts cover 46 working rows and one uncertified combined view.
Shared project selections, pull request mutations and repository discovery appear
in their tables below and above rather than being counted twice here.

Field expansions used below (connection `nodes` contain the stated fields):

- **List**: `number,title,url,state,updatedAt,labels(first:100){nodes{id,name,description,color},totalCount}`;
  list/search additionally select `stateReason`.
- **Author**: `author{login,...on User{id,name}}`.
- **Comment**: `id,author{login,...on User{id,name}},authorAssociation,body,createdAt,includesCreatedEdit,isMinimized,minimizedReason,reactionGroups{content,users{totalCount}},url,viewerDidAuthor`.
  `CommentsForIssue` selects only `author{login}`; `lastComment` omits `id,url,viewerDidAuthor`.
- **Issue JSON**: `assignees(first:100){nodes{id,login,name,databaseId},totalCount}`, Author,
  `body,closed,createdAt,closedAt,id,number,state,title,updatedAt,url`,
  labels as above, `milestone{number,title,description,dueOn}`, Comment via
  `comments(first:100){nodes{…},pageInfo{hasNextPage,endCursor},totalCount}`,
  `reactionGroups{content,users{totalCount}}`, `isPinned,stateReason`,
  `issueType{id,name,description,color}`, `parent{id,number,title,url,state,repository{nameWithOwner}}`,
  `subIssues(first:100){nodes{…same linked issue fields…},totalCount}`,
  `subIssuesSummary{total,completed,percentCompleted}`,
  `blockedBy(first:50)` and `blocking(first:50)` with linked issue nodes and `totalCount`,
  `closedByPullRequestsReferences(first:100){nodes{id,number,url,repository{id,name,owner{id,login}}},pageInfo{hasNextPage,endCursor}}`,
  `projectCards(first:100){nodes{project{name},column{name}},totalCount}`,
  `projectItems(first:100){nodes{id,project{id,title},status:fieldValueByName(name:"Status"){...on ProjectV2ItemFieldSingleSelectValue{optionId,name}}},totalCount}`.
- **Lookup** (`IssueByNumber`): `repository{hasIssuesEnabled,issue:issueOrPullRequest(number:){__typename,...on Issue{…},...on PullRequest{…}}}`.
  Pull-request fragments omit issue-only fields. View fetches `projectItems`
  separately as `IssueProjectItems`; list/status include it in the fragment.
- **Page**: `pageInfo{hasNextPage,endCursor}`.

| Command | Operation name | Selected fields | Status |
|---|---|---|---|
| issue create, transfer | IssueRepositoryInfo | repository: id,databaseId,name,owner.login,hasIssuesEnabled,viewerPermission | works |
| issue create | IssueCreate | createIssue.issue: id,url | works |
| issue create with assignees/labels/milestone | IssueCreate | same; assigneeIds, labelIds, milestoneId inputs | works |
| issue edit/close/reopen/comment/delete/transfer/develop; view --json subsets | IssueByNumber | Lookup; requested scalar fields, Author, labels, assignees, milestone, Comment, reactions, types and relationships | works |
| issue view (default), view/list/status --json all | IssueByNumber / IssueList / IssueStatus / IssueSearch | Issue JSON; default view also lastComment, types, parent, subIssues, summary, dependencies, projects: [view default](../tests/repositories/graphql/cli/issue-view-default.graphql), [view all](../tests/repositories/graphql/cli/issue-view-all.graphql), [list all](../tests/repositories/graphql/cli/issue-list-all.graphql), [search all](../tests/repositories/graphql/cli/issue-search-all.graphql), [status all](../tests/repositories/graphql/cli/issue-status-all.graphql) | works |
| issue list | IssueList | fragment issue = List + stateReason; repository.hasIssuesEnabled,issues.totalCount,nodes,Page; states/assignee/author/mention filters | works |
| issue list --search/--label/--milestone | IssueSearch | same fragment; repository.hasIssuesEnabled,search.issueCount,nodes,Page; ISSUE or ISSUE_ADVANCED | works |
| issue status | UserCurrent | viewer.login | works |
| issue status | IssueStatus | fragment issue = List; repository.hasIssuesEnabled,assigned/mentioned/authored issues: totalCount,nodes | works |
| issue view --comments, comment --edit-last/--delete-last | CommentsForIssue | node: Issue and PullRequest fragments, comments(first:100,after:){nodes{Comment},totalCount,Page} | works |
| issue view --json closedByPullRequestsReferences | closedByPullRequestsReferences | node Issue fragment: closedByPullRequestsReferences nodes{id,number,url,repository{id,name,owner{id,login}}},Page | works |
| issue comment, close/reopen --comment | CommentCreate | addComment.commentEdge.node.url | works |
| issue comment --edit-last | CommentUpdate | updateIssueComment.issueComment.url | works |
| issue comment --delete-last | CommentDelete | deleteIssueComment.clientMutationId | works |
| issue close | IssueClose | closeIssue.issue.id (stateReason input supported) | works |
| issue close --reason duplicate --duplicate-of | IssueClose | same; duplicateIssueId input | works |
| issue reopen | IssueReopen | reopenIssue.issue.id | works |
| issue edit --title/--body | IssueUpdate | updateIssue.__typename | works |
| issue edit --add-assignee/--remove-assignee/--milestone; metadata replacement | IssueUpdate | updateIssue.__typename; assigneeIds/labelIds/milestoneId inputs (null removes the milestone) | works |
| issue edit --add-label | LabelAdd | addLabelsToLabelable.__typename | works |
| issue edit --remove-label | LabelRemove | removeLabelsFromLabelable.__typename | works |
| issue lock | LockLockable | lockLockable.lockedRecord.locked | works |
| issue unlock | UnlockLockable | unlockLockable.unlockedRecord.locked | works |
| issue pin | IssuePin | pinIssue.issue.id | works |
| issue unpin | IssueUnpin | unpinIssue.issue.id | works |
| issue delete | IssueDelete | deleteIssue.repository.id | works |
| issue transfer | IssueTransfer | transferIssue.issue.url | works |
| issue create/edit --type | RepositoryIssueTypes | repository.issueTypes(first:50).nodes: id,name,description,color | works |
| issue create/edit --type/--remove-type | UpdateIssueIssueType | updateIssueIssueType.issue.id | works |
| issue create/edit relationship flags | IssueNodeID | repository.issue(number:).id | works |
| issue create/edit --parent/--add-sub-issue | AddSubIssue | addSubIssue.issue.id; replaceParent input | works |
| issue edit --remove-parent/--remove-sub-issue | RemoveSubIssue | removeSubIssue.issue.id | works |
| issue create/edit --blocked-by/--blocking | AddBlockedBy | addBlockedBy.issue.id (blocking swaps arguments) | works |
| issue edit --remove-blocked-by/--remove-blocking | RemoveBlockedBy | removeBlockedBy.issue.id | works |
| issue create/edit metadata | RepositoryAssignableUsers | repository.assignableUsers(first:100,after:): nodes{id,login,name},Page | works |
| issue create/edit labels | RepositoryLabelList | repository.labels(first:100,orderBy:NAME ASC,after:): nodes{id,name},Page | works |
| issue create/edit milestone | RepositoryMilestoneList | repository.milestones(states:,first:100,after:): nodes{id,title},Page | works |
| issue list --milestone number | RepositoryMilestoneByNumber | repository.milestone(number:): id,title | works |
| issue create interactive/--template | IssueTemplates | repository.issueTemplates: name,body,title | works |
| issue develop | LinkedBranchFeature | LinkedBranch:__type.fields.name | works |
| issue develop --list, existing branch lookup | ListLinkedBranches | repository.issue.linkedBranches(first:30).nodes.ref: name,repository.url | works |
| issue develop | FindRepoBranchID | repository.id,defaultBranchRef.target.oid,ref(qualifiedName:).target.oid | works |
| issue develop | CreateLinkedBranch | createLinkedBranch.linkedBranch: id,ref.name | works |
| issue relationship detection | Issue_fields | Issue:__type.fields(includeDeprecated:true).name | works |
| issue project detection | Repository_fields | Repository:__type.fields(includeDeprecated:true).name | works |
| issue search detection | SearchType_enumValues | SearchType:__type.enumValues(includeDeprecated:true).name | works |
| label list/clone | LabelList | fragment label: color,description,name (or --json color,createdAt,description,id,isDefault,name,updatedAt,url); labels.totalCount,nodes,Page; query/orderBy arguments | works |

Tests: `tests/repositories/graphql/gh_issues.rs`, selected by `gh_issues_queries`,
through the real router on SQLite and PostgreSQL. They cover metadata replacement,
duplicate targets and visibility, templates, administrator-only deletion, same-owner
transfers and redirects, the three-pin limit and native forms, closing keywords and
linked branch publication. Project selections and mutations have their own router
contracts below; combined default/all-fields issue views remain uncertified.
Builder job `950dfa132ef34855b5268518fb1c698c` passed formatting and Clippy with
warnings denied, then reported:
`test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 231 filtered out; finished in 4.40s`.

# gh request compatibility

## gh run, gh workflow, gh cache, gh secret, gh variable

Traced from gh 2.102.0 (`pkg/cmd/{run,workflow,cache,secret,variable}`, their shared
clients, `api/queries_repo.go`, and `internal/featuredetection`). Paths below are
relative to `/api/v3`; GraphQL operations use `POST /api/graphql`. Repository selection
is explicit (`-R HOST/OWNER/REPO`); general remote discovery is shared with other commands.
Placeholders are URL-encoded. Optional query parameters and body fields are in brackets.
Paginated REST clients follow `Link: rel="next"`, except workflow listing, which increments
`page` until the returned page is short. `N` is the requested limit capped at 100.
Interactive run selectors request 10 or 50 runs; workflow view requests five. Browser
flags construct a web URL from the same metadata; they do not call another API.

Response abbreviations:

- **Run:** `name`, `display_title`, `created_at`, `updated_at`, `run_started_at`, `status`,
  `conclusion`, `event`, `id`, `workflow_id`, `run_number`, `run_attempt`, `head_branch`,
  `jobs_url`, `head_commit.message`, `head_sha`, `html_url`,
  `head_repository.{name,owner.login}`. Run lists also read `total_count` and `workflow_runs`.
- **Workflow:** `name`, `id`, `path`, `state`, inside `workflows` for lists. gh resolves
  selectors by numeric id or filename directly, or by name through the workflow list.
- **Job:** `id`, `status`, `conclusion`, `name`, `started_at`, `completed_at`, `html_url`,
  `run_id`, `steps[].{name,status,conclusion,number,started_at,completed_at}`. Lists read
  `jobs` and `total_count`.
- **Cache:** `id`, `key`, `ref`, `version`, `created_at`, `last_accessed_at`, `size_in_bytes`,
  in `actions_caches`; listings and key deletion read `total_count`.
- **Artifact:** `name`, `size_in_bytes`, `archive_download_url`, `expired`, inside `artifacts`.
- **Secret:** `name`, `updated_at`; organization/user results also read `visibility` and
  `selected_repositories_url`. Selected-repository counts supply `num_selected_repos` locally.
- **Variable:** `name`, `value`, `created_at`, `updated_at`; organization results also read
  `visibility` and `selected_repositories_url`. Selected-repository counts are computed locally.

`A` below means `/repos/{owner}/{repo}/actions`, `E` means
`/repos/{owner}/{repo}/environments/{environment}`, and `O` means `/orgs/{org}/actions`.
The repository secret application defaults to `actions`; `--app dependabot` and
`--app codespaces` replace that path component. No command in these groups sends a
GraphQL mutation. `--env-file` repeats the corresponding set request for each name.

| Command | Request and fields read | Status |
|---|---|---|
| run list; interactive run view/watch/cancel/rerun/delete | `GET A/runs?per_page=N&exclude_pull_requests=true[&branch=…&actor=…&status=…&event=…&created=…&head_sha=…]`; Run list | works |
| run list --workflow; workflow view | `GET A/workflows/{id}/runs?per_page=N&exclude_pull_requests=true` with the same optional filters; Run list | works |
| run list/view/watch/cancel/rerun/delete; workflow list/view/run/enable/disable | `GET A/workflows?per_page=N&page=P` (default workflow list N=50; all-name preloads N=100); Workflow | works |
| run view/watch/cancel/rerun/delete; workflow view/run/enable/disable | `GET A/workflows/{id-or-filename}`; Workflow | works |
| run view/watch/cancel/rerun/delete | `GET A/runs/{id}?exclude_pull_requests=true`; Run | works |
| run view --attempt | `GET A/runs/{id}/attempts/{attempt}?exclude_pull_requests=true`; Run | works; numbered attempts share a REST run id |
| run view/watch | `GET {jobs_url}?per_page=100`; Job list | works for observed native executions |
| run view --attempt | `GET A/runs/{id}/attempts/{attempt}/jobs?per_page=100`; Job list | works for observed native attempts |
| run view --job; run rerun --job | `GET A/jobs/{job_id}`; Job | works for observed native jobs |
| run view/watch | GraphQL **PullRequestForRun** below; `pullRequests.nodes[].{number,headRepository.{name,owner.login}}` | works |
| run view/watch annotations | `GET /repos/{owner}/{repo}/check-runs/{job_id}/annotations`; array of `{message,path,annotation_level,start_line}` (404 is tolerated by gh) | works for native runner error/warning/notice commands |
| run view --log/--log-failed | `GET A/runs/{id}/logs` or `GET A/runs/{id}/attempts/{attempt}/logs`; ZIP archive, job/step filenames and line text | works |
| run view --log/--log-failed fallback | `GET A/jobs/{job_id}/logs`; redirect and text log | works |
| run cancel | `POST A/runs/{id}/cancel`, no body; success status only | works |
| run cancel --force | `POST A/runs/{id}/force-cancel`, no body; success status only | works; cancellation terminates the process group, including always jobs |
| run rerun | `POST A/runs/{id}/rerun`, no body; success status only | works |
| run rerun --debug | `POST A/runs/{id}/rerun`, body `{enable_debug_logging:true}`; success status only | works |
| run rerun --failed | `POST A/runs/{id}/rerun-failed-jobs`, no body or `{enable_debug_logging:true}`; success status only | works for observed native executions |
| run rerun --job | `POST A/jobs/{job_id}/rerun`, no body or `{enable_debug_logging:true}`; success status only | works for observed native executions |
| run delete | `DELETE A/runs/{id}`, no body; success status only | works |
| run download | `GET A/artifacts?per_page=100` or `GET A/runs/{id}/artifacts?per_page=100`; Artifact | works |
| run download | `GET {archive_download_url}` then `GET {Location}` without repository credentials; redirect and ZIP bytes | works |
| workflow view --yaml [--ref]; workflow run interactive | `GET /repos/{owner}/{repo}/contents/{workflow.path}[?ref=…]`; base64 `content` | works |
| workflow run without --ref; workflow view --web --yaml without --ref | GraphQL **RepositoryInfo** below; `defaultBranchRef.name` (the full requested fragment must resolve) | works |
| workflow run | `GET /meta`; `installed_version` selects the dispatch response shape | works |
| workflow run | `POST A/workflows/{id}/dispatches`, body `{ref,inputs:{name:string,…}}`; 204 has no fields; on GHES >=3.21 gh also sends `return_run_details:true` and reads `{workflow_run_id,run_url,html_url}` if returned | works (GHES 3.20 response shape) |
| workflow disable | `PUT A/workflows/{id}/disable`, no body; success status only | works |
| workflow enable | `PUT A/workflows/{id}/enable`, no body; success status only | works |
| cache list | `GET A/caches?per_page=N&sort=last_accessed_at&direction=desc[&key=…&ref=…]`; `--sort` also sends `created_at`/`size_in_bytes`, `--order` sends `asc`/`desc`; Cache | works |
| cache delete --all | `GET A/caches?per_page=100[&ref=…]`, then `DELETE A/caches/{id}` for every result; Cache ids and success status | works |
| cache delete ID | `DELETE A/caches/{id}`, no body; 204 only | works |
| cache delete KEY | `DELETE A/caches?key=…[&ref=…]`, no body; `total_count` | works |
| secret list | `GET A/secrets?per_page=100`; `secrets[]` Secret | works |
| secret set; secret set --no-store | `GET A/secrets/public-key`; `key_id`, base64 `key`; --no-store stops after local sealed-box encryption | works |
| secret set | `PUT A/secrets/{name}`, body `{key_id,encrypted_value}` (base64 anonymous sealed box); 201/204 only | works |
| secret delete | `DELETE A/secrets/{name}`, no body; 204 only | works |
| secret list --org | `GET /orgs/{org}/{app}/secrets?per_page=100`; Secret; optionally `GET {selected_repositories_url}` reads `total_count` | works for Actions and Dependabot; Codespaces out of scope |
| secret set --org | `GET /orgs/{org}/{app}/secrets/public-key`; `key_id,key`; then `PUT /orgs/{org}/{app}/secrets/{name}`, body `{key_id,encrypted_value,visibility[,selected_repository_ids]}`; Dependabot ids are strings, others integers | works for Actions and Dependabot; Codespaces out of scope |
| secret delete --org | `DELETE /orgs/{org}/{app}/secrets/{name}`; success status only | works for Actions and Dependabot; Codespaces out of scope |
| secret list --env | `GET E/secrets?per_page=100`; Secret | works |
| secret set --env | `GET E/secrets/public-key`; `key_id,key`; then `PUT E/secrets/{name}`, body `{key_id,encrypted_value}` | works |
| secret delete --env | `DELETE E/secrets/{name}`; success status only | works |
| secret list --app dependabot/codespaces | `GET /repos/{owner}/{repo}/{app}/secrets?per_page=100`; Secret | works for Dependabot; Codespaces out of scope |
| secret set --app dependabot/codespaces | `GET /repos/{owner}/{repo}/{app}/secrets/public-key`, then `PUT /repos/{owner}/{repo}/{app}/secrets/{name}`, body `{key_id,encrypted_value}` | works for Dependabot; Codespaces out of scope |
| secret delete --app dependabot/codespaces | `DELETE /repos/{owner}/{repo}/{app}/secrets/{name}`; success status only | works for Dependabot; Codespaces out of scope |
| secret list --user | `GET /user/codespaces/secrets?per_page=100`; Secret; optionally `GET {selected_repositories_url}` reads `total_count` | fails: no Codespaces |
| secret set --user | `GET /user/codespaces/secrets/public-key`, then `PUT /user/codespaces/secrets/{name}`, body `{key_id,encrypted_value[,selected_repository_ids]}` | fails: no Codespaces |
| secret delete --user | `DELETE /user/codespaces/secrets/{name}`; success status only | fails: no Codespaces |
| secret set --repos; variable set --repos/--env | GraphQL **MapRepositoryNames** below; aliased `databaseId` for each named repository | works |
| variable list | `GET A/variables?per_page=100`; `variables[]` Variable | works |
| variable get | `GET A/variables/{name}`; Variable | works |
| variable set | `POST A/variables`, body `{name,value}`; on 409 only, `PATCH A/variables/{name}`, body `{value}`; 201/204 only | works |
| variable set empty value | gh omits `value`: `POST A/variables` body `{name}`, or `PATCH A/variables/{name}` body `{}` | works; omitted value creates/clears an empty value; name-only renames retain it |
| variable delete | `DELETE A/variables/{name}`; 204 only | works |
| variable list/get --org | `GET O/variables?per_page=100` or `GET O/variables/{name}`; Variable; optionally `GET {selected_repositories_url}` reads `total_count` | works |
| variable set --org | `POST O/variables`, body `{name,value,visibility[,selected_repository_ids]}`; on 409 `PATCH O/variables/{name}`, body `{value,visibility[,selected_repository_ids]}` (empty values omitted) | works |
| variable delete --org | `DELETE O/variables/{name}`; success status only | works |
| variable list/get --env | `GET E/variables?per_page=100` or `GET E/variables/{name}`; Variable | works |
| variable set --env | after MapRepositoryNames, `POST /repositories/{repository_id}/environments/{env}/variables`, body `{name,value}`; on 409 `PATCH …/variables/{name}`, body `{value}` (empty values omitted) | works |
| variable delete --env | `DELETE E/variables/{name}`; success status only | works |

**PullRequestForRun** (run view/watch, including closed/merged PRs; gh compares the
returned head repository to the run's head repository):

```graphql
query PullRequestForRun($owner: String!, $repo: String!, $headRefName: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequests(headRefName: $headRefName, first: 1, orderBy: { field: CREATED_AT, direction: DESC }) {
      nodes {
        number
        headRepository {
          owner { login }
          name
        }
      }
    }
  }
}
```

**RepositoryInfo** (the shared default-branch client requests this whole document):

```graphql
fragment repo on Repository {
  id
  databaseId
  name
  owner { login }
  sshUrl
  hasIssuesEnabled
  description
  hasWikiEnabled
  viewerPermission
  defaultBranchRef { name }
}
query RepositoryInfo($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    ...repo
    parent { ...repo }
    mergeCommitAllowed
    rebaseMergeAllowed
    squashMergeAllowed
  }
}
```

**MapRepositoryNames** (generated aliases and string literals repeat for each repository):

```graphql
query MapRepositoryNames {
  repo_000: repository(owner: "OWNER", name: "REPO") {
    databaseId
  }
}
```

Codeyard reports GHES **3.20.0 compatibility** in `/meta`, not a GitHub installation
or Codeyard product version. This selects gh's bodyless 204 workflow-dispatch response.
Workflow ids are durable database identities. Runs link to the existing UUID-based Actions
pages; reruns retain a shared REST run id with numbered historical attempts. Job, step,
annotation and selective-rerun support uses authenticated native runner observations.
Historical runs and engines without observations return an explicit 404 for job metadata,
not fabricated whole-run jobs. Annotations retain at most 1,000 diagnostics per job.

SQLite/PostgreSQL router contracts are in `tests/ci/actions_{runs,workflows,jobs,logs,reruns,
run_mutations}.rs`, `action_{secrets,variables}.rs`, `organization_values.rs`,
`environment_values.rs`, `dependabot.rs` and `gh_apps.rs`. Job contracts check every field gh
reads, archives and credential-free log redirects. Real native reruns exercise failed/job
selection, debug contexts and command annotations with masking and stop-commands. Variable
contracts send gh's omitted-value payloads and numeric-repository environment URL, including
create-conflict-patch upserts.
# gh CLI compatibility

## gh ruleset, gh gist, gh ssh-key, gh gpg-key, gh attestation, gh auth status

Traced from gh 2.102.0 (`pkg/cmd/{ruleset,gist,ssh-key,gpg-key,attestation,auth/status}`,
`api/queries_user.go`, `api/queries_repo.go`, and `pkg/cmd/auth/shared/oauth_scopes.go`).
REST paths below are relative to `/api/v3`; GraphQL requests POST `{query,variables}`
to `/api/graphql`. REST requests have no query parameters or body unless shown.
`{id}`, `{owner}`, `{repo}`, `{org}` and `{branch}` are escaped path components.
Shared requests appear once, with all their callers named. Browser navigation and
Git subprocesses are identified separately, not counted as working API requests.

| Command | Request and response fields read | Status |
|---|---|---|
| `ruleset list`; interactive `ruleset view` (repository) | `RepoRulesetList` below. Variables: `owner`, `repo`, `includeParents` (default true), `limit=min(requested,100)` (default requested 30), then `endCursor` on subsequent pages. Reads `level.rulesets.{totalCount,nodes,pageInfo}` and every field selected below. | works: [`Repository.rulesets`](../tests/repositories/graphql/cli/ruleset-list-repo.graphql) lists the stored repository rulesets and, with `includeParents`, the organization rulesets that apply; tested in `tests/organizations/github/repo_rulesets.rs`. |
| `ruleset list --org`; interactive `ruleset view --org` | `OrgRulesetList` below. Variables: `login`, `includeParents`, `limit`, subsequent `endCursor`; pagination and response fields as above. | works: [`Organization.rulesets`](../tests/repositories/graphql/cli/ruleset-list-org.graphql) lists the organization's rulesets. |
| `ruleset view ID` (also `--web`) | `GET /repos/{owner}/{repo}/rulesets/{id}`. Reads `id`, `name`, `enforcement`, `source`, `source_type`, `current_user_can_bypass`, `bypass_actors[].{actor_id,actor_type,bypass_mode}`, all nested `conditions` keys/values, `rules[].{type,parameters}`, `_links.html.href` for `--web`. Interactive mode first lists rulesets, and uses the organization endpoint if the selected source is an organization. | works: native ruleset documents; `current_user_can_bypass` is omitted because rulesets are not enforced. |
| `ruleset view ID --org ORG` (also `--web`) | `GET /orgs/{org}/rulesets/{id}`; same fields as repository view. | works: native ruleset documents; `current_user_can_bypass` is omitted because rulesets are not enforced. |
| `ruleset check [BRANCH]` | `GET /repos/{owner}/{repo}/rules/branches/{branch}`. Reads array length and `[].{type,parameters,ruleset_source_type,ruleset_source,ruleset_id}`. `--default` first sends `RepositoryInfo` below and uses `repository.defaultBranchRef.name`. Without a branch, uses local Git. `--web` opens `/{owner}/{repo}/rules?ref=<encoded refs/heads/branch>` instead of the REST request (still resolves `--default`). | works: returns the branch's protection rules, which are the rules Codeyard enforces; the `ruleset_*` source fields are empty. |
| `gist list`; interactive `gist view`, `gist edit`, `gist delete` | `GistList` below. Variables: `per_page=min(limit,100)` (default 10), `endCursor=null` then returned cursor, `visibility=ALL/PUBLIC/SECRET`, `includeContent` (default false). Reads every selected field; filters locally and follows `pageInfo.hasNextPage/endCursor`. | works: native gists, privacy filters, creation ordering and cursor pagination. |
| `gist create` | `POST /gists`; body `description`, `public`, `files` map (`content`, optional `filename/type/language/raw_url/truncated`), and Go's zero `updated_at` value (`0001-01-01T00:00:00Z`). Reads `html_url` for output/browser/clipboard. | works: native Git-backed creation. |
| `gist view`, `gist edit`, `gist rename`, `gist delete` with ID/URL | `GET /gists/{id}`. Reads `id`, `description`, `files` map and its filenames, `files.*.{filename,content,type,truncated,raw_url}`, `owner.login` for edit/rename. `view --web` with an explicit ID/URL opens a browser without this request. | works: complete file metadata and bounded inline text. |
| `gist view` (truncated file) | `GET <files.*.raw_url>` (absolute URL); reads raw response bytes. | works: immutable raw-file URLs supply complete bytes. |
| `gist edit` | After selection/GET and `UserCurrent`, `POST /gists/{id}` (not PATCH); body `description`, `files` map with `{content,filename?}` or null for deletions. Decodes a gist response but uses no fields after the update. | works: POST shares the native update and write checks. |
| `gist rename` | After GET and `UserCurrent`, `POST /gists/{id}` (not PATCH); body contains `description`, `files` with old entry `{content:""}` and renamed entry `{filename:<new>,content,...retained file metadata}`, `public:false`, zero `updated_at`. Decodes a gist response but uses no fields after the update. | works: gh's old-file marker deletes the old name; the zero public field does not change visibility. |
| `gist delete` | After GET or interactive listing, `DELETE /gists/{id}`; reads status only. | works: native deletion. |
| `gist clone` | No REST or GraphQL. Runs Git clone of `https://HOST/gist/ID.git` or `git@HOST:gist/ID.git`, or the supplied URL. | works: `/gist/ID.git` aliases the shared HTTP/SSH transport; preexisting ordinary repositories keep their names. |
| `ssh-key list`; `ssh-key add --type authentication` preflight | `GET /user/keys?per_page=100`; reads `[].{id,key,title,created_at}` (add compares only `key`, without the OpenSSH comment). Does not follow pagination. | works: router contract passes on SQLite and PostgreSQL. |
| `ssh-key list`; `ssh-key add --type signing` preflight | `GET /user/ssh_signing_keys?per_page=100`; same fields and pagination behavior as authentication keys. `type` in list output is assigned locally, not read from the API. | works: router contract passes on SQLite and PostgreSQL. |
| `ssh-key add` (default authentication) | After preflight, `POST /user/keys`; body `{title,key}` with default `title:""` and full key-file text. Reads status only. A matching key in preflight skips POST. | works: empty titles receive a native default; router contract passes on both engines. |
| `ssh-key add --type signing` | After preflight, `POST /user/ssh_signing_keys`; body and duplicate behavior as above; reads status only. | works: empty titles receive a native default; router contract passes on both engines. |
| `ssh-key delete ID` (including `--yes`) | `GET /user/keys/{id}`; reads `title` for confirmation/output. gh does not offer signing-key deletion. | works: router contract passes on SQLite and PostgreSQL. |
| `ssh-key delete ID` | After GET, `DELETE /user/keys/{id}`; reads status only. | works: router contract passes on SQLite and PostgreSQL. |
| `gpg-key list`; `gpg-key delete` lookup | `GET /user/gpg_keys?per_page=100`; list reads `[].{key_id,public_key,emails[].email,created_at,expires_at}`; delete reads `[].{id,key_id}` to translate the supplied GPG key ID to a database ID. Does not follow pagination. | works: native OpenPGP keys supply every list and lookup field. |
| `gpg-key add` | `POST /user/gpg_keys`; body `armored_public_key`, optional `name` when title is nonempty. Reads status; errors inspect `errors[].{field,message}` for duplicate `key_id`. | works: verified OpenPGP enrollment and gh's duplicate-key error shape. |
| `gpg-key delete KEY-ID` | After lookup, `DELETE /user/gpg_keys/{id}`; reads status only. | works: native deletion under live credential authority. |
| `auth status`; `gist edit`, `gist rename` ownership prerequisite | `query UserCurrent{viewer{login}}`, `variables` omitted; reads `data.viewer.login`. Auth status sends it for environment-sourced tokens only; configured usernames come from the local config. | works: router contract passes on SQLite and PostgreSQL. |
| `auth status` (including `--active`, `--show-token`, `--json`) | `GET /` (wire URL `/api/v3/`), `Authorization: token <token>`; requires 200 and reads `X-OAuth-Scopes`, discarding the body. Repeats per selected account. Other output fields come from local config/environment, not API fields. | works: router contract passes on SQLite and PostgreSQL; bad/revoked tokens fail. |
| `attestation download` | No request to Codeyard: `auth.IsHostSupported` rejects GHES before constructing the request. | fails: gh itself reports “gh attestation does not currently support GHES”, even though Codeyard now has attestation REST endpoints. |
| `attestation verify` (including local bundle/OCI options) | No request to Codeyard: same unconditional GHES host check. | fails: gh rejects GHES before verification. |
| `attestation inspect` | No request to Codeyard: same GHES host check, then would read a local bundle. | fails: gh rejects GHES. |
| `attestation trusted-root` | No request to Codeyard: same GHES host check. | fails: gh rejects GHES. |

Table totals: **24 works / 4 fails**. Rulesets are stored and listed but not enforced; attestation commands
are blocked by gh's GHES host restriction, not fake-success responses.

Gist, GPG-key, SSH-key and auth-status requests use native features. The exact-request router
contracts are `tests/identity/gh_gists_gpg.rs` and `tests/identity/gh_keys_auth.rs`, on SQLite
and PostgreSQL. Gist clone IDs use the singular `/gist/` namespace that gh constructs;
existing `/gists/` clone URLs continue to work.
The CLI does not send public-user SSH/GPG list requests from these subcommands (their shared
helpers accept a user argument, but these callers pass the empty string), nor does it send
signing-key GET/DELETE, gist PATCH, or attestation creation requests. Attestation's cloud-only
client can request `/repos/{owner}/{repo}/attestations/{digest}` or
`/orgs/{org}/attestations/{digest}` with `per_page` and optional `predicate_type`, external
bundle URLs, and `/meta` for tenancy trust domains, but those are not requests to a GHES host
and are not server work for this task.

### Exact GraphQL operations

The `UserCurrent` and `GistList` wire queries were also confirmed with `GH_DEBUG=api`
against a refused loopback connection. `UserCurrent` is the exact single-line query shown
in the table. The ruleset queries are
literal strings in `pkg/cmd/ruleset/shared/http.go` (the header and body are concatenated):

```graphql

query RepoRulesetList($limit: Int!, $endCursor: String, $includeParents: Boolean, $owner: String!, $repo: String!) {
	level: repository(owner: $owner, name: $repo) {

rulesets(first: $limit, after: $endCursor, includeParents: $includeParents) {
	totalCount
	nodes {
		databaseId
		name
		target
		enforcement
		source {
			__typename
			... on Repository { owner: nameWithOwner }
			... on Organization { owner: login }
		}
		rules {
			totalCount
		}
	}
	pageInfo {
		hasNextPage
		endCursor
	}
}}}
```

```graphql

query OrgRulesetList($limit: Int!, $endCursor: String, $includeParents: Boolean, $login: String!) {
	level: organization(login: $login) {

rulesets(first: $limit, after: $endCursor, includeParents: $includeParents) {
	totalCount
	nodes {
		databaseId
		name
		target
		enforcement
		source {
			__typename
			... on Repository { owner: nameWithOwner }
			... on Organization { owner: login }
		}
		rules {
			totalCount
		}
	}
	pageInfo {
		hasNextPage
		endCursor
	}
}}}
```

`GistList` is generated from the struct in `pkg/cmd/gist/shared/shared.go` (variable
declarations are sorted by the Go GraphQL client):

```graphql
query GistList($endCursor:String$includeContent:Boolean!$per_page:Int!$visibility:GistPrivacy!){viewer{gists(first: $per_page, after: $endCursor, privacy: $visibility, orderBy: {field: CREATED_AT, direction: DESC}){nodes{description,files{name,text @include(if: $includeContent)},isPublic,name,updatedAt},pageInfo{hasNextPage,endCursor}}}}
```

`RepositoryInfo` is literal text from `api/queries_repo.go`; only `defaultBranchRef.name`
is subsequently used by `ruleset check --default`, but the entire selection must resolve:

```graphql

	fragment repo on Repository {
		id
		databaseId
		name
		owner { login }
		sshUrl
		hasIssuesEnabled
		description
		hasWikiEnabled
		viewerPermission
		defaultBranchRef {
			name
		}
	}

	query RepositoryInfo($owner: String!, $name: String!) {
		repository(owner: $owner, name: $name) {
			...repo
			parent {
				...repo
			}
			mergeCommitAllowed
			rebaseMergeAllowed
			squashMergeAllowed
		}
	}
```
# GitHub CLI GraphQL compatibility

## gh status, gh search

Queries traced against gh 2.102.0. No mutations are sent by these commands.
The exact wire queries (including `AssignedSearch`'s two fragments) are exercised
through `/api/graphql` on SQLite and PostgreSQL in
`tests/repositories/graphql_status.rs`. All three operations pass on both engines
(builder job `a898b43d649b42b2a8a0c1c47538b1b1`).

| Command | Operation name | Fields selected | Status |
| --- | --- | --- | --- |
| `gh status` (all flags) | `UserCurrent` | `viewer { login }` | works |
| `gh status` (including `--org`, repeated `--exclude`) | `AssignedSearch` | `assignments: search(first: $limit, type: ISSUE, query: $searchAssigns) { nodes { ...issue ...pr } }`, `reviewRequested: search(first: $limit, type: ISSUE, query: $searchReviews) { nodes { ...pr } }`; fragments `issue on Issue` and `pr on PullRequest` both select `__typename updatedAt title number repository { nameWithOwner }` | works |
| `gh search issues`, `gh search prs` (including `--include-prs`, `--json`, semantic/hybrid flags) | `SearchType_enumValues` | `SearchType: __type(name: "SearchType") { enumValues(includeDeprecated: true) { name } }` | works |

`UserCurrent` comes from `api/queries_user.go:CurrentLoginName` via status's
`CurrentUsername`. `AssignedSearch` is `pkg/cmd/status/status.go:searchQuery`;
flags change its variables, not its selection. The default limit is 25.
Assignments include issues and pull requests; review requests include pull
requests with a current stored reviewer request. Both exclude closed items,
archived repositories, and repositories the viewer cannot read.

Search feature detection is in
`internal/featuredetection/feature_detection.go:SearchFeatures`. Its introspection
runs when advanced search is available (on Enterprise, after a REST version
probe). Search results themselves always use REST via
`pkg/search/searcher.go:search`: `search/issues` for issues and PRs,
`search/repositories`, `search/code`, and `search/commits`. `--json`, `--jq`,
and `--template` select local output fields, not GraphQL fields. `--web`
opens a URL without a search request. `gh search repos`, `code`, and `commits`
send no GraphQL operations. Status's notifications, mention bodies, and
received events also use REST. Their REST rows are below; wire requests and every
field decoded by gh are covered in `tests/collaboration/gh_status_search.rs` on
SQLite and PostgreSQL.

## gh discussion

Traced from gh 2.102.0's `pkg/cmd/discussion/{client,create,edit,list,view,comment,shared}`.
These commands use GraphQL only. Explicit repository selection avoids the shared remote
resolution queries already covered above. **16 works / 0 fails**: the router contracts in
`tests/repositories/graphql/discussion.rs` cover these selections on SQLite and PostgreSQL.

Enable discussions with `updateRepository(input:{repositoryId,hasDiscussionsEnabled:true})`.
Categories are seeded on first read. Discussions share issue/PR numbering; comments support
one level of replies. Writers create and comment, authors or triagers edit, and maintainers
choose answers. Reactions, upvotes, polls, category management and close/reopen are not implemented.

Selection abbreviations preserve all fields chosen by the client:

- `A`: `__typename,login,...on User{id,name},...on Bot{id}`.
- `R`: `reactionGroups{content,users{totalCount}}`.
- `D`: `id,number,title,body,url,closed,stateReason,author{A},category{id,name,slug,emoji,isAnswerable},
  labels(first:20){nodes{id,name,color}},isAnswered,answerChosenAt,answerChosenBy{A},R,
  createdAt,updatedAt,closedAt,locked`.
- `C`: `id,url,author{A},body,createdAt,isAnswer,upvoteCount,R`.
- `P`: `endCursor,hasNextPage,startCursor,hasPreviousPage`.

| Command | Operation | Selected fields | Status |
| --- | --- | --- | --- |
| create; view/edit/comment by number; view/comment with a numeric comment URL | `RepositoryMetaForDiscussions` | `repository{id,databaseId,hasDiscussionsEnabled}` | works |
| create --label; edit --add-label/--remove-label | `RepositoryLabelsForDiscussions` | `repository.labels(first:100,after,orderBy:NAME_ASC){nodes{id,name,color},pageInfo{hasNextPage,endCursor}}` | works |
| create; edit category; list --category | `DiscussionCategoryList` | `repository{hasDiscussionsEnabled,discussionCategories(first:100){nodes{id,name,slug,emoji,isAnswerable}}}` | works |
| list (including --json, --answered, --state, --sort, --order, --after) | `DiscussionList` | `repository{hasDiscussionsEnabled,discussions(first,after,orderBy,categoryId,states,answered){totalCount,pageInfo{hasNextPage,endCursor},nodes{D}}}` | works |
| list --author/--label/--search | `DiscussionListSearch` | `search(type:DISCUSSION,query,first,after){discussionCount,pageInfo{hasNextPage,endCursor},nodes{...on Discussion{D}}}` | works |
| view without comments; edit; comment NUMBER | `DiscussionMinimal` | `repository.discussion(number){D,comments{totalCount}}` | works |
| view --comments; view --json comments (oldest/newest, pagination) | `DiscussionWithComments` | `repository.discussion(number){D,comments(first,last,after,before){totalCount,pageInfo{P},nodes{C,replies(last:4){totalCount,nodes{C}}}}}` | works |
| view COMMENT-ID/COMMENT-URL (oldest/newest, pagination) | `DiscussionCommentReplies` | `node(id:$commentID){...on DiscussionComment{C,discussion{D},replies(first,last,after,before){totalCount,pageInfo{P},nodes{C}}}}` | works |
| comment reply/edit/delete; view COMMENT --web | `GetDiscussionComment` | `node(id){__typename,...on DiscussionComment{C,discussion{id}}}` | works |
| create | `CreateDiscussion` | `createDiscussion(input){discussion{D}}`; input repositoryId,categoryId,title,body | works |
| edit title/body/category | `UpdateDiscussion` | `updateDiscussion(input){discussion{D}}`; input discussionId and changed title/body/categoryId | works |
| edit --remove-label | `RemoveLabelsFromDiscussion` | `removeLabelsFromLabelable(input){labelable{...on Discussion{D}}}` | works |
| create --label; edit --add-label | `AddLabelsToDiscussion` | `addLabelsToLabelable(input){labelable{...on Discussion{D}}}` | works |
| comment NUMBER; comment COMMENT-ID/COMMENT-URL | `AddDiscussionComment` | `addDiscussionComment(input){comment{C}}`; input discussionId,body, optional replyToId | works |
| comment --edit | `UpdateDiscussionComment` | `updateDiscussionComment(input){comment{C}}`; input commentId,body | works |
| comment --delete (including --yes) | `DeleteDiscussionComment` | `deleteDiscussionComment(input){comment{id}}`; input id | works |

`list --web` and `view NUMBER --web` only construct browser URLs; they send no discussion
query. `view COMMENT --web` resolves the comment URL using GetDiscussionComment. Numeric
comment URLs first use RepositoryMetaForDiscussions, then gh constructs a `DC_` node ID by
base64url-encoding MessagePack `[0,repositoryDatabaseId,commentDatabaseId]` locally. JSON output
flags do not narrow the list selection; view requests the comments query only when exporting
comments. Labels are removed before additions, in separate mutations after create/update.

## gh status and search REST requests

| Command | REST request | Qualifiers built from flags |
| --- | --- | --- |
| `gh status` | `GET /notifications?all=true&participating=true&per_page=100`, then `subject.latest_comment_url` | Organization and excluded repositories are filtered locally; mentions include read threads. |
| Current user | `GET /user` | No qualifiers; status currently gets its login through `UserCurrent` instead. |
| `gh status` activity | `GET /users/{login}/received_events?per_page=100` | Reads issue, pull-request and comment event payloads, repository name, organization login and creation time; repository event feeds share the projection. |
| `gh search repos` | `GET /search/repositories` | `archived`, `created`, `followers`, `fork`, `forks`, `good-first-issues`, `help-wanted-issues`, `in:name,description,readme`, `is:public,private,internal`, `language`, `license`, `pushed`, `size`, `stars`, `topic`, `topics`, `user` |
| `gh search issues` | `GET /search/issues` | `type:issue` (omitted with `--include-prs`), `archived`, `assignee`, `author` (including `app/{slug}`), `closed`, `commenter`, `comments`, `created`, `in:title,body,comments`, `interactions`, `involves`, `is:public,private,internal,locked,unlocked`, `label`, `language`, `mentions`, `milestone`, `no:assignee,label,milestone,project`, `project`, `reactions`, `repo`, `state`, `team`, `updated`, `user` |
| `gh search prs` | `GET /search/issues` | Issue qualifiers with `type:pr`, plus `base`, `draft`, `head`, `is:merged,unmerged`, `merged`, `review`, `review-requested`, `reviewed-by`, `status`, `team-review-requested` |
| `gh search commits` | `GET /search/commits` | `author`, `author-date`, `author-email`, `author-name`, `committer`, `committer-date`, `committer-email`, `committer-name`, `hash`, `is:public,private,internal`, `merge`, `parent`, `repo`, `tree`, `user` |
| `gh search code` | `GET /search/code`, `Accept: application/vnd.github.text-match+json` | `extension`, `filename`, `in:file,path`, `language`, `repo`, `size`, `user`; code snippets include match text and byte indices. |

Search sends `q`, `page=1`, `per_page=min(limit,100)`, `order` and optional
`sort`, then follows `Link`. Advanced issue queries group repeated fields and
owners with `OR`. Semantic/hybrid requests explicitly report lexical fallback.
There is no internal visibility tier, so `is:internal` matches nothing. README
and license search pin the default branch and use the same license recognition
as the license API; they scan at most 250 repositories and report
`incomplete_results` when the time, file or memory bounds are reached.

# GitHub GraphQL compatibility

## gh project

Inventory for gh 2.102.0 (`pkg/cmd/project/**`, its `shared/queries` package,
`api/queries_repo.go`, `api/queries_org.go`, and ProjectV2 feature detection).
Each row is a distinct wire document. Inline fragments are included in the
fixtures under `tests/repositories/graphql/projects/queries/`; `RepositoryInfo`
also includes its named `repo` fragment. `graphql_projects_cli_sqlite` and
`graphql_projects_cli_postgres` send all 46 documents through the real router,
check every selected response key, and assert stored values and mutation effects.
Both passed on builder job `d23550916dee405c84ccb75d7dea3113`.

Selection abbreviations below expand as follows (connection arguments are
preserved verbatim in the fixtures):

- **Field**: `__typename`; on ProjectV2Field and ProjectV2IterationField:
  `id name dataType`; on ProjectV2SingleSelectField: `id name dataType options { id name }`.
- **Item**: `id content { __typename }`; DraftIssue content: `id body title`;
  Issue/PullRequest content: `body title number url repository { nameWithOwner }`;
  `fieldValues(first:100) { nodes { __typename } }`, with these inline fragments,
  each also selecting `field { Field }`: date (`date`), iteration
  (`title startDate duration iterationId`), label (`labels(first:10) { nodes { name } }`),
  number (`number`), single-select (`name`), text (`text`), milestone
  (`milestone { title description dueOn }`), pull request
  (`pullRequests(first:10) { nodes { url } }`), repository (`repository { url }`),
  user (`users(first:10) { nodes { login } }`), reviewer
  (`reviewers(first:10) { nodes { __typename ... on Team { name } ... on User { login } } }`).
- **Project**: `number url shortDescription public closed title id readme`,
  `owner { __typename ... on User { login } ... on Organization { login } }`,
  `fields { totalCount nodes { Field } pageInfo { endCursor hasNextPage } }`,
  `items { pageInfo { endCursor hasNextPage } totalCount nodes { Item } }`.
- **Mutation project**: Project's scalar and owner selections, but only
  `items { totalCount } fields { totalCount }` (gh supplies `first:0`).
- **Repo**: `id databaseId name owner { login } sshUrl hasIssuesEnabled description
  hasWikiEnabled viewerPermission defaultBranchRef { name }`.

Project commands use `--format json`, not `--json`. Formatting flags (`--format`,
`--jq`, `--template`), `list --closed`, field columns (`--field`, `--field-id`),
and pagination limits do not change selections. `view --web` still fetches the
project; `list --web` only resolves the owner. `--undo` changes mutation inputs
for close, and switches mutation documents for archive/template. Copy's `--drafts`
changes its input only. Selecting an owner or project interactively uses the
same owner/list documents below. Named item editing adds the URL lookup and
field-list documents. `item-list --query` adds introspection and a query argument;
its three owner variants have separate fixture rows.

“Works” describes these wire selections over Codeyard's stored project types.
The native filter language supports title text, type/state and custom-field
predicates, not all GitHub filter operators (such as assignee/label predicates).
Every project also lists GitHub's built-in Assignees, Labels, Linked pull requests,
Milestone, Repository and Reviewers fields, and items return their label, milestone,
repository, user, reviewer and linked-PR values from the issue or pull request itself.
`list --web` constructs a github.com URL inside gh itself, independently of GraphQL.

| Command / path | Operation name | Selected fields | Status |
| --- | --- | --- | --- |
| `list --web --owner @me` | `Viewer` | `viewer { login id }` | works |
| Owner `@me` preflight | `ViewerOwner` | `viewer { login id }` | works |
| Named owner preflight | `UserOrgOwner` | `user(login) { login id } organization(login) { login id }` (one NOT_FOUND error, one populated object) | works |
| Interactive owner preflight | `ViewerLoginAndOrgs` | `viewer { login id organizations(first:100,after) { pageInfo { endCursor hasNextPage } nodes { login viewerCanCreateProjects id } } }` | works |
| `view`, mutation preflight, interactive project selection | `ViewerProject` | `viewer { projectV2(number) { Project } login }` | works |
| Same, user owner | `UserProject` | `user(login) { projectV2(number) { Project } login }` | works |
| Same, organization owner | `OrgProject` | `organization(login) { projectV2(number) { Project } login }` | works |
| `list`, interactive project selection | `ViewerProjects` | `viewer { projectsV2(first,after) { totalCount pageInfo { endCursor hasNextPage } nodes { Project } } login }` | works |
| Same, user owner | `UserProjects` | `user(login) { projectsV2 { totalCount pageInfo { endCursor hasNextPage } nodes { Project } } login }` | works |
| Same, organization owner | `OrgProjects` | `organization(login) { projectsV2 { totalCount pageInfo { endCursor hasNextPage } nodes { Project } } login }` | works |
| `field-list`, named `item-edit`, extra item columns beyond first field page | `ViewerProjectWithFields` | `viewer { projectV2(number) { Project } }` | works |
| Same, user owner | `UserProjectWithFields` | `user(login) { projectV2(number) { Project } }` | works |
| Same, organization owner | `OrgProjectWithFields` | `organization(login) { projectV2(number) { Project } }` | works |
| `item-list` | `ViewerProjectWithItems` (without query) | `viewer { projectV2(number) { Project } }` | works |
| Same, user owner | `UserProjectWithItems` (without query) | `user(login) { projectV2(number) { Project } }` | works |
| Same, organization owner | `OrgProjectWithItems` (without query) | `organization(login) { projectV2(number) { Project } }` | works |
| `item-list --query` | `ProjectV2_fields` | `ProjectV2: __type(name:"ProjectV2") { fields(includeDeprecated:true) { name args { name } } }` | works |
| `item-list --query`, viewer | `ViewerProjectWithItems` (with query) | Project, with `items(first,after,query)` | works |
| Same, user owner | `UserProjectWithItems` (with query) | Project, with `items(first,after,query)` | works |
| Same, organization owner | `OrgProjectWithItems` (with query) | Project, with `items(first,after,query)` | works |
| `create` | `CreateProjectV2` | `createProjectV2 { projectV2 { Mutation project } }` | works |
| `edit` | `UpdateProjectV2` | `updateProjectV2 { projectV2 { Mutation project } }` | works |
| `close`, `close --undo` | `CloseProjectV2` | `updateProjectV2 { projectV2 { Mutation project } }` | works |
| `copy` | `CopyProjectV2` | `copyProjectV2 { projectV2 { Mutation project } }` | works |
| `delete` | `DeleteProject` | `deleteProjectV2 { projectV2 { Mutation project } }` | works |
| `mark-template` | `MarkProjectTemplate` | `markProjectV2AsTemplate { projectV2 { Mutation project } }` | works |
| `mark-template --undo` | `UnmarkProjectTemplate` | `unmarkProjectV2AsTemplate { projectV2 { Mutation project } }` | works |
| `field-create` (TEXT, NUMBER, DATE, SINGLE_SELECT) | `CreateField` | `createProjectV2Field { projectV2Field { Field } }` | works |
| `field-delete` | `DeleteField` | `deleteProjectV2Field { projectV2Field { Field } }` | works |
| `item-create` | `CreateDraftItem` | `addProjectV2DraftIssue { projectItem { Item } }` | works |
| `item-add` URL preflight | `GetIssueOrPullRequest` | `resource(url) { __typename ... on Issue { id } ... on PullRequest { id } }` | works |
| `item-add` | `AddItem` | `addProjectV2ItemById { item { Item } }` | works |
| Named `item-edit` URL preflight | `GetProjectItemByURL` | `resource(url) { __typename ... on Issue { projectItems(first) { nodes { id project { id } } } } ... on PullRequest { projectItems(first) { nodes { id project { id } } } } }` | works |
| `item-edit` draft preflight (only title or body supplied) | `DraftIssueByID` | `node(id) { ... on DraftIssue { id body title } }` | works |
| `item-edit --title/--body` | `EditDraftIssueItem` | `updateProjectV2DraftIssue { draftIssue { id body title } }` | works |
| `item-edit` typed value / named value | `UpdateItemValues` | `updateProjectV2ItemFieldValue { projectV2Item { Item } }` | works |
| `item-edit --clear` | `ClearItemFieldValue` | `clearProjectV2ItemFieldValue { projectV2Item { Item } }` | works |
| `item-archive` | `ArchiveProjectItem` | `archiveProjectV2Item { item { Item } }` | works |
| `item-archive --undo` | `UnarchiveProjectItem` | `unarchiveProjectV2Item { item { Item } }` | works |
| `item-delete` | `DeleteProjectItem` | `deleteProjectV2Item { deletedItemId }` | works |
| `link/unlink --repo` repository preflight | `RepositoryInfo` | `repository { Repo parent { Repo } mergeCommitAllowed rebaseMergeAllowed squashMergeAllowed }` | works |
| `link --repo` | `LinkProjectV2ToRepository` | `linkProjectV2ToRepository { clientMutationId }` | works |
| `unlink --repo` | `UnlinkProjectV2FromRepository` | `unlinkProjectV2FromRepository { clientMutationId }` | works |
| `link/unlink --team` team preflight | `OrganizationTeam` | `organization(login) { team(slug) { id slug } }` | works |
| `link --team` | `LinkProjectV2ToTeam` | `linkProjectV2ToTeam { clientMutationId }` | works |
| `unlink --team` | `UnlinkProjectV2FromTeam` | `unlinkProjectV2FromTeam { clientMutationId }` | works |

## gh issue and pr project metadata

These gh 2.102.0 documents come from `api/queries_projects_v2.go` and
`api/queries_{repo,org}.go`. Fixtures live beside the project command queries;
`graphql_projects_metadata_{sqlite,postgres}` check them through the real router,
including separate aliased mutation inputs, permissions, status values and cursors.
This covers project metadata, not the unrelated fields used by issue/PR commands.

| Path | Operation | Selected fields | Status |
| --- | --- | --- | --- |
| Issue create/edit/view project lookup | `IssueProjectItems` | `projectItems { totalCount nodes { id project { id title } status:fieldValueByName(name:"Status") { ... on ProjectV2ItemFieldSingleSelectValue { optionId name } } } pageInfo }` | works |
| PR create/edit/view project lookup | `PullRequestProjectItems` | Same selection on PullRequest | works |
| Repository project choices | `RepositoryProjectV2List` | `projectsV2(first:100,orderBy:{field:TITLE,direction:ASC},after,query:"is:open") { nodes { id title number resourcePath closed url } pageInfo }` | works |
| Organization project choices | `OrganizationProjectV2List` | Same connection on Organization | works |
| Current user's project choices | `UserProjectV2List` | Same connection on viewer (User) | works |
| Retired classic project choices | `RepositoryProjectList`, `OrganizationProjectList` | `projects(states:[OPEN],first:100,orderBy:{field:NAME,direction:ASC},after) { nodes { id name number resourcePath } pageInfo }` | works (empty) |
| Add/remove projects | `UpdateProjectV2Items` | Multiple aliased `addProjectV2ItemById { item { id } }` / `deleteProjectV2Item { deletedItemId }`, each with its own input | works |
