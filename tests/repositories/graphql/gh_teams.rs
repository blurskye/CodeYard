//! Query texts from gh 2.102.0's release, org and repo command paths.
use super::{call, query, request};
use crate::support::{database::Database, http_client::Client, organizations::applied};
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db,
    identity::tokens::{self, TokenInput},
    repositories::lifecycle,
    storage::ObjectStore,
};
use serde_json::{json, Value};
use std::sync::Arc;

const RELEASE_FIELDS: &str =
    r#"query Release_fields{Release: __type(name: "Release"){fields{name}}}"#;
const REPOSITORY_FIELDS: &str = r#"query Repository_fields{Repository: __type(name: "Repository"){fields(includeDeprecated: true){name}}}"#;
const USER_CURRENT: &str = "query UserCurrent{viewer{login}}";
const RELEASE_LIST: &str = "query RepositoryReleaseList($direction:OrderDirection!$endCursor:String$name:String!$owner:String!$perPage:Int!){repository(owner: $owner, name: $name){releases(first: $perPage, orderBy: {field: CREATED_AT, direction: $direction}, after: $endCursor){nodes{name,tagName,isDraft,immutable,isLatest,isPrerelease,createdAt,publishedAt},pageInfo{hasNextPage,endCursor}}}}";
const RELEASE_LIST_OLD: &str = "query RepositoryReleaseList($direction:OrderDirection!$endCursor:String$name:String!$owner:String!$perPage:Int!){repository(owner: $owner, name: $name){releases(first: $perPage, orderBy: {field: CREATED_AT, direction: $direction}, after: $endCursor){nodes{name,tagName,isDraft,isLatest,isPrerelease,createdAt,publishedAt},pageInfo{hasNextPage,endCursor}}}}";
const FIND_REF: &str = "query RepositoryFindRef($name:String!$owner:String!$tagName:String!){repository(owner: $owner, name: $name){ref(qualifiedName: $tagName){id}}}";
const RELEASE_BY_TAG: &str = "query RepositoryReleaseByTag($name:String!$owner:String!$tagName:String!){repository(owner: $owner, name: $name){release(tagName: $tagName){databaseId,isDraft}}}";
const ORG_LIST: &str = r#"query OrganizationList($user: String!, $limit: Int!, $endCursor: String) {
		user(login: $user) {
			login
			organizations(first: $limit, after: $endCursor) {
				totalCount
				nodes {
					login
				}
				pageInfo {
					hasNextPage
					endCursor
				}
			}
		}
	}"#;
pub(super) const LIST_VIEWER: &str = r#"query RepositoryList($perPage:Int!,$endCursor:String,$privacy:RepositoryPrivacy,$fork:Boolean) {
		repositoryOwner: viewer {
			login
			repositories(first: $perPage, after: $endCursor, privacy: $privacy, isFork: $fork, ownerAffiliations: OWNER, orderBy: { field: PUSHED_AT, direction: DESC }) {
				nodes{nameWithOwner,description,isPrivate,isFork,isArchived,createdAt,pushedAt,visibility}
				totalCount
				pageInfo{hasNextPage,endCursor}
			}
		}
	}"#;
pub(super) const LIST_OWNER: &str = r#"query RepositoryList($perPage:Int!,$endCursor:String,$privacy:RepositoryPrivacy,$fork:Boolean,$owner:String!) {
		repositoryOwner(login: $owner) {
			login
			repositories(first: $perPage, after: $endCursor, privacy: $privacy, isFork: $fork, ownerAffiliations: OWNER, orderBy: { field: PUSHED_AT, direction: DESC }) {
				nodes{nameWithOwner,description,isPrivate,isFork,isArchived,createdAt,pushedAt,visibility}
				totalCount
				pageInfo{hasNextPage,endCursor}
			}
		}
	}"#;
pub(super) const LIST_SEARCH: &str = r#"query RepositoryListSearch($query:String!,$perPage:Int!,$endCursor:String) {
		search(type: REPOSITORY, query: $query, first: $perPage, after: $endCursor) {
			repositoryCount
			nodes{...on Repository{nameWithOwner,description,isPrivate,isFork,isArchived,createdAt,pushedAt,visibility}}
			pageInfo{hasNextPage,endCursor}
		}
	}"#;
pub(super) const DEFAULT_FIELDS: &str =
    "nameWithOwner,description,isPrivate,isFork,isArchived,createdAt,pushedAt,visibility";
const EDIT_FIELDS: &str = "defaultBranchRef{name},deleteBranchOnMerge,description,hasIssuesEnabled,hasProjectsEnabled,hasWikiEnabled,homepageUrl,isInOrganization,isTemplate,mergeCommitAllowed,rebaseMergeAllowed,repositoryTopics(first:100){nodes{topic{name}}},stargazerCount,squashMergeAllowed,watchers{totalCount},visibility,autoMergeAllowed";
const EDIT: &str = r#"query RepositoryInfo($owner: String!, $name: String!) {
		repository(owner: $owner, name: $name) {%s}
	}"#;
const NETWORK: &str = r#"
	fragment repo on Repository {
		id
		databaseId
		name
		owner { login }
		viewerPermission
		defaultBranchRef {
			name
		}
		isPrivate
	}
	query RepositoryNetwork {
		viewer { login }
		
		repo_000: repository(owner: "teams-alice", name: "proj") {
			...repo
			parent {
				...repo
			}
		}
		
	}
	"#;

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("teams-alice").await;
    let bob = f.actor("teams-bob").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        Some("CLI queries"),
        false,
        None,
    )
    .await
    .unwrap();
    lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "secret",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let org = applied(
        db::organizations::profiles::create_organization(
            f.pool(),
            alice,
            &db::organizations::profiles::NewOrganization::new(
                "teams-org",
                db::organizations::profiles::OrganizationProfile::new(Some("Teams"), None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: alice,
            target: db::repositories::creation::RepositoryTarget::Organization(org.id),
        },
        "org-repo",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::topics::update_about(
        f.pool(),
        alice,
        repo.id,
        "CLI queries",
        Some("https://example.test"),
        &["rust".to_owned()],
    )
    .await
    .unwrap();
    let router = create_router(super::patient(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    )));
    let mut credentials = Vec::new();
    for actor in [alice, bob] {
        let session = Client::login(&router, f.pool(), actor).await;
        credentials.push(
            tokens::create(
                f.pool(),
                actor,
                Some(&session.session),
                TokenInput {
                    name: "gh teams",
                    scope: "write",
                    expires_at: None,
                },
            )
            .await
            .unwrap()
            .value,
        );
    }
    let token = &credentials[0];
    let outsider = &credentials[1];
    let (status, saved) = request(
        &router,
        Some(token),
        "PUT",
        "/api/v3/repos/teams-alice/proj/contents/main.rs",
        json!({"message":"Initial commit","content":"Zm4gbWFpbigpIHt9Cg=="}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let (status, tag) = request(
        &router,
        Some(token),
        "POST",
        "/api/v3/repos/teams-alice/proj/git/refs",
        json!({"ref":"refs/tags/v1","sha":saved["commit"]["sha"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{tag}");
    let mut releases = Vec::new();
    for (tag, draft, prerelease) in [
        ("v1", false, false),
        ("preview", false, true),
        ("pending", true, false),
    ] {
        releases.push(
            db::releases::create_release(
                f.pool(),
                alice,
                repo.id,
                tag,
                "main",
                &db::releases::ReleaseFields {
                    name: Some(tag.into()),
                    is_draft: draft,
                    is_prerelease: prerelease,
                    make_latest: (tag == "v1").into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
    }
    // Publishing an older draft must not change CREATED_AT ordering.
    f.execute("UPDATE releases SET created_at='2020-01-01T00:00:00Z', published_at='2030-01-01T00:00:00Z' WHERE id=$1", &[releases[0].id]).await;
    let vars = json!({"owner":"teams-alice","name":"proj","direction":"DESC","perPage":1,"endCursor":null});
    for (text, field) in [
        (RELEASE_FIELDS, "Release"),
        (REPOSITORY_FIELDS, "Repository"),
    ] {
        let data = query(&router, token, text, json!({})).await;
        let fields = data[field]["fields"].as_array().unwrap();
        assert!(fields.iter().all(|value| value["name"].is_string()));
        assert!(fields.iter().any(|value| value["name"]
            == if field == "Release" {
                "immutable"
            } else {
                "visibility"
            }));
    }
    assert_eq!(
        query(&router, token, USER_CURRENT, json!({})).await,
        json!({"viewer":{"login":"teams-alice"}})
    );
    for text in [RELEASE_LIST, RELEASE_LIST_OLD] {
        let data = query(&router, token, text, vars.clone()).await;
        let list = &data["repository"]["releases"];
        let node = &list["nodes"][0];
        assert_eq!(node["name"], "pending");
        assert_eq!(node["tagName"], "pending");
        assert_eq!(node["isDraft"], true);
        assert_eq!(node["isLatest"], false);
        assert_eq!(node["isPrerelease"], false);
        assert_eq!(node["publishedAt"], Value::Null);
        assert!(node["createdAt"].is_string());
        assert_eq!(
            node.as_object().unwrap().len(),
            if text == RELEASE_LIST { 8 } else { 7 }
        );
        if text == RELEASE_LIST {
            assert_eq!(node["immutable"], false);
        }
        assert_eq!(list["pageInfo"]["hasNextPage"], true);
        let mut next = vars.clone();
        next["endCursor"] = list["pageInfo"]["endCursor"].clone();
        let data = query(&router, token, text, next).await;
        assert_eq!(
            data["repository"]["releases"]["nodes"][0]["tagName"],
            "preview"
        );
        let mut ascending = vars.clone();
        ascending["direction"] = json!("ASC");
        assert_eq!(
            query(&router, token, text, ascending).await["repository"]["releases"]["nodes"][0]
                ["tagName"],
            "v1"
        );
        assert_eq!(
            query(&router, outsider, text, vars.clone()).await["repository"]["releases"]["nodes"]
                [0]["tagName"],
            "preview"
        );
    }
    let mut by_tag = json!({"owner":"teams-alice","name":"proj","tagName":"pending"});
    assert_eq!(
        query(&router, token, RELEASE_BY_TAG, by_tag.clone()).await,
        json!({"repository":{"release":{"databaseId":releases[2].id,"isDraft":true}}})
    );
    assert_eq!(
        query(&router, outsider, RELEASE_BY_TAG, by_tag.clone()).await,
        json!({"repository":{"release":null}})
    );
    by_tag["tagName"] = json!("missing");
    assert_eq!(
        query(&router, token, RELEASE_BY_TAG, by_tag.clone()).await,
        json!({"repository":{"release":null}})
    );
    by_tag["tagName"] = json!("refs/tags/v1");
    let data = query(&router, token, FIND_REF, by_tag.clone()).await;
    assert!(!data["repository"]["ref"]["id"].as_str().unwrap().is_empty());
    let refs = query(
        &router,
        token,
        "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){id defaultBranchRef{id name target{oid} repository{id}} ref(qualifiedName:\"refs/tags/v1\"){id name target{oid} repository{id}}}}",
        by_tag.clone(),
    )
    .await;
    let repository = &refs["repository"];
    for field in ["defaultBranchRef", "ref"] {
        assert!(repository[field]["id"].is_string());
        assert_eq!(repository[field]["repository"]["id"], repository["id"]);
        assert_eq!(repository[field]["target"]["oid"], saved["commit"]["sha"]);
    }
    assert_eq!(repository["defaultBranchRef"]["name"], "main");
    assert_eq!(repository["ref"]["name"], "v1");
    assert_eq!(repository["ref"]["id"], data["repository"]["ref"]["id"]);
    assert_ne!(
        repository["defaultBranchRef"]["id"],
        repository["ref"]["id"]
    );
    by_tag["tagName"] = json!("refs/tags/missing");
    assert_eq!(
        query(&router, token, FIND_REF, by_tag).await,
        json!({"repository":{"ref":null}})
    );
    let org_vars = json!({"user":"teams-alice","limit":1});
    let data = query(&router, token, ORG_LIST, org_vars.clone()).await;
    assert_eq!(data["user"]["login"], "teams-alice");
    assert_eq!(data["user"]["organizations"]["totalCount"], 1);
    assert_eq!(
        data["user"]["organizations"]["nodes"],
        json!([{"login":"teams-org"}])
    );
    assert_eq!(
        data["user"]["organizations"]["pageInfo"]["hasNextPage"],
        false
    );
    assert!(data["user"]["organizations"]["pageInfo"]["endCursor"].is_string());
    assert_eq!(
        query(&router, outsider, ORG_LIST, org_vars).await["user"]["organizations"]["totalCount"],
        0
    );
    let list_vars = json!({"owner":"teams-alice","perPage":1});
    for text in [LIST_VIEWER, LIST_OWNER] {
        let data = query(&router, token, text, list_vars.clone()).await;
        let list = &data["repositoryOwner"]["repositories"];
        assert_eq!(data["repositoryOwner"]["login"], "teams-alice");
        assert_eq!(list["totalCount"], 2);
        assert_eq!(list["nodes"][0].as_object().unwrap().len(), 8);
        for key in ["nameWithOwner", "createdAt"] {
            assert!(list["nodes"][0][key].is_string());
        }
        for key in ["isPrivate", "isFork", "isArchived"] {
            assert!(list["nodes"][0][key].is_boolean());
        }
        assert_eq!(list["pageInfo"]["hasNextPage"], true);
        let mut next = list_vars.clone();
        next["endCursor"] = list["pageInfo"]["endCursor"].clone();
        let next = query(&router, token, text, next).await;
        assert_ne!(
            list["nodes"][0]["nameWithOwner"],
            next["repositoryOwner"]["repositories"]["nodes"][0]["nameWithOwner"]
        );
        assert_eq!(
            next["repositoryOwner"]["repositories"]["pageInfo"]["hasNextPage"],
            false
        );
        let mut private = list_vars.clone();
        private["privacy"] = json!("PRIVATE");
        assert_eq!(
            query(&router, token, text, private).await["repositoryOwner"]["repositories"]["nodes"]
                [0]["nameWithOwner"],
            "teams-alice/secret"
        );
        let mut forks = list_vars.clone();
        forks["fork"] = json!(true);
        assert_eq!(
            query(&router, token, text, forks).await["repositoryOwner"]["repositories"]
                ["totalCount"],
            0
        );
    }
    assert_eq!(
        query(&router, outsider, LIST_OWNER, list_vars.clone()).await["repositoryOwner"]
            ["repositories"]["totalCount"],
        1
    );
    assert_eq!(
        query(
            &router,
            token,
            LIST_OWNER,
            json!({"owner":"teams-org","perPage":1})
        )
        .await["repositoryOwner"]["repositories"]["nodes"][0]["nameWithOwner"],
        "teams-org/org-repo"
    );
    assert_eq!(
        query(
            &router,
            outsider,
            LIST_OWNER,
            json!({"owner":"teams-org","perPage":1})
        )
        .await["repositoryOwner"]["repositories"]["totalCount"],
        0
    );
    for (filter, count) in [
        (
            "sort:updated-desc archived:false fork:true topic:rust user:@me",
            1,
        ),
        ("sort:updated-desc archived:true fork:true user:@me", 0),
        ("sort:updated-desc fork:true is:internal user:@me", 0),
        ("sort:updated-desc fork:true language:Rust user:@me", 1),
        ("sort:updated-desc fork:only user:@me", 0),
        ("sort:updated-desc fork:false is:private user:@me", 1),
    ] {
        let data = query(
            &router,
            token,
            LIST_SEARCH,
            json!({"query":filter,"perPage":1}),
        )
        .await;
        assert_eq!(data["search"]["repositoryCount"], count, "{filter}: {data}");
        assert_eq!(
            data["search"]["nodes"].as_array().unwrap().len(),
            count as usize
        );
        assert_eq!(data["search"]["pageInfo"]["hasNextPage"], false);
    }
    let repo_vars = json!({"owner":"teams-alice","name":"proj"});
    for omitted in [
        ",visibility",
        ",autoMergeAllowed",
        ",visibility,autoMergeAllowed",
    ] {
        let fields = EDIT_FIELDS.replace(omitted, "");
        let data = query(
            &router,
            token,
            &EDIT.replace("%s", &fields),
            repo_vars.clone(),
        )
        .await;
        assert_eq!(
            data["repository"].as_object().unwrap().len(),
            fields.split(',').count()
        );
    }
    let edit = query(
        &router,
        token,
        &EDIT.replace("%s", EDIT_FIELDS),
        repo_vars.clone(),
    )
    .await;
    assert_eq!(
        edit["repository"],
        json!({
            "defaultBranchRef":{"name":"main"}, "deleteBranchOnMerge":false, "description":"CLI queries",
            "hasIssuesEnabled":true, "hasProjectsEnabled":true, "hasWikiEnabled":false,
            "homepageUrl":"https://example.test", "isInOrganization":false, "isTemplate":false,
            "mergeCommitAllowed":true, "rebaseMergeAllowed":true, "squashMergeAllowed":true,
            "repositoryTopics":{"nodes":[{"topic":{"name":"rust"}}]}, "stargazerCount":0,
            "watchers":{"totalCount":0}, "visibility":"PUBLIC", "autoMergeAllowed":false,
        })
    );
    for (credential, expected) in [(token, true), (outsider, false)] {
        assert_eq!(
            query(
                &router,
                credential,
                &EDIT.replace("%s", "viewerCanAdminister"),
                repo_vars.clone()
            )
            .await,
            json!({"repository":{"viewerCanAdminister":expected}})
        );
    }
    // --json uses the same two list templates with RepositoryGraphQL's selected fields.
    let json_fields = "id,name,nameWithOwner,description,homepageUrl,url,sshUrl,createdAt,pushedAt,updatedAt,hasIssuesEnabled,hasProjectsEnabled,hasWikiEnabled,mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed,forkCount,stargazerCount,deleteBranchOnMerge,isArchived,isFork,isInOrganization,isPrivate,visibility,isTemplate,viewerCanAdminister,viewerPermission,owner{id,login},parent{id,name,owner{id,login}},defaultBranchRef{name},latestRelease{publishedAt,tagName,name,url},repositoryTopics(first:100){nodes{topic{name}}},watchers{totalCount},labels(first:100){nodes{id,color,name,description}}";
    for text in [LIST_OWNER, LIST_VIEWER, LIST_SEARCH] {
        let vars = json!({"owner":"teams-alice","perPage":10,"privacy":"PUBLIC","query":"sort:updated-desc fork:true is:public user:@me"});
        let data = query(
            &router,
            token,
            &text.replace(DEFAULT_FIELDS, json_fields),
            vars,
        )
        .await;
        let node = if text == LIST_SEARCH {
            &data["search"]["nodes"][0]
        } else {
            &data["repositoryOwner"]["repositories"]["nodes"][0]
        };
        assert_eq!(node.as_object().unwrap().len(), 34);
        for field in [
            "id",
            "name",
            "nameWithOwner",
            "description",
            "homepageUrl",
            "url",
            "sshUrl",
            "createdAt",
            "updatedAt",
            "visibility",
            "viewerPermission",
        ] {
            assert!(node[field].is_string(), "{field}: {node}");
        }
        assert!(node["pushedAt"].is_null() || node["pushedAt"].is_string());
        for field in [
            "hasIssuesEnabled",
            "hasProjectsEnabled",
            "hasWikiEnabled",
            "mergeCommitAllowed",
            "squashMergeAllowed",
            "rebaseMergeAllowed",
            "deleteBranchOnMerge",
            "isArchived",
            "isFork",
            "isInOrganization",
            "isPrivate",
            "isTemplate",
            "viewerCanAdminister",
        ] {
            assert!(node[field].is_boolean(), "{field}: {node}");
        }
        assert_eq!(node["forkCount"], 0);
        assert_eq!(node["stargazerCount"], 0);
        assert_eq!(node["watchers"], json!({"totalCount":0}));
        assert_eq!(node["labels"], json!({"nodes":[]}));
        assert_eq!(node["name"], "proj");
        assert_eq!(node["parent"], Value::Null);
        assert_eq!(node["owner"]["login"], "teams-alice");
        assert!(node["owner"]["id"].is_string());
        assert_eq!(node["viewerPermission"], "ADMIN");
        assert_eq!(node["latestRelease"]["tagName"], "v1");
        assert_eq!(node["latestRelease"]["name"], "v1");
        assert!(node["latestRelease"]["publishedAt"].is_string());
        assert!(node["latestRelease"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/teams-alice/proj/releases/tag/v1"));
        assert_eq!(node["defaultBranchRef"]["name"], "main");
        assert_eq!(
            node["repositoryTopics"]["nodes"],
            json!([{"topic":{"name":"rust"}}])
        );
    }
    let data = query(&router, token, NETWORK, json!({})).await;
    assert_eq!(data["viewer"], json!({"login":"teams-alice"}));
    assert_eq!(data["repo_000"]["databaseId"], repo.id);
    assert_eq!(data["repo_000"]["name"], "proj");
    assert_eq!(data["repo_000"]["parent"], Value::Null);
    assert_eq!(data["repo_000"]["viewerPermission"], "ADMIN");
    assert_eq!(data["repo_000"]["owner"], json!({"login":"teams-alice"}));
    assert_eq!(data["repo_000"]["defaultBranchRef"], json!({"name":"main"}));
    assert_eq!(data["repo_000"]["isPrivate"], false);
    assert!(data["repo_000"]["id"].is_string());
    let broadcaster = codeyard_notifications::NotificationBroadcaster::new(f.pool());
    lifecycle::fork(
        f.pool(),
        None,
        root.path(),
        &broadcaster,
        bob,
        repo.id,
        "teams-alice",
    )
    .await
    .unwrap();
    let fork_network = NETWORK.replace("\"teams-alice\"", "\"teams-bob\"");
    let data = query(&router, outsider, &fork_network, json!({})).await;
    assert_eq!(data["repo_000"]["owner"]["login"], "teams-bob");
    assert_eq!(data["repo_000"]["parent"]["name"], "proj");
    assert_eq!(data["repo_000"]["parent"]["databaseId"], repo.id);
    assert_eq!(data["repo_000"]["parent"]["owner"]["login"], "teams-alice");
    assert_eq!(data["repo_000"]["parent"]["viewerPermission"], "READ");
    assert_eq!(data["repo_000"]["parent"]["isPrivate"], false);
    assert_eq!(
        data["repo_000"]["parent"]["defaultBranchRef"]["name"],
        "main"
    );
    assert!(data["repo_000"]["parent"]["id"].is_string());
    let (_, denied) = call(
        &router,
        Some(outsider),
        &EDIT.replace("%s", "viewerCanAdminister"),
        json!({"owner":"teams-alice","name":"secret"}),
    )
    .await;
    assert_eq!(denied["data"]["repository"], Value::Null);
    assert_eq!(denied["errors"][0]["type"], "NOT_FOUND");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_gh_teams_queries() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_gh_teams_queries() {
    contract(true).await;
}
