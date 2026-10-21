//! The team page at /orgs/NAME/teams/SLUG. Only active organization members see it; anyone else
//! gets the answer of a missing team, and a secret team stays with its members and owners and admins.
use super::fixture::{applied, Fixture, Reply};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::{
    backend::db::{models::OrgRole, organizations::*},
    webui::{app::Page, bootstrap, pages::orgs::TeamPage},
};
use tower::ServiceExt;

const TEAMS: &str = "/orgs/native_org/teams";

fn team_page(reply: &Reply) -> TeamPage {
    match reply.envelope().page {
        Page::OrganizationTeam(page) => page,
        other => panic!("expected team page, got {other:?}"),
    }
}

fn repository_names(page: &TeamPage) -> Vec<String> {
    let mut names: Vec<String> = page
        .repositories
        .iter()
        .map(|grant| grant.name.clone())
        .collect();
    names.sort();
    names
}

// No session cookie: the viewer is anonymous.
async fn anonymous(f: &Fixture, path: &str, model: bool) -> Reply {
    let mut request = Request::builder().method("GET").uri(path);
    if model {
        request = request.header(header::ACCEPT, bootstrap::MODEL_CONTENT_TYPE);
    }
    let response = f
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8(
        to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    Reply {
        status,
        headers,
        body,
    }
}

pub async fn contracts(f: &Fixture) {
    let pool = f.database.pool();
    let bystander = Fixture::actor(pool, "org_bystander").await;
    applied(
        add_organization_member(pool, f.owner.id, f.org, bystander.id, OrgRole::Member)
            .await
            .unwrap(),
    );
    // Signed in, but not a member of the organization.
    let stranger = Fixture::actor(pool, "stranger").await;
    let visible = f.team("writers", TeamPrivacy::Visible).await;
    let secret = f.team("council", TeamPrivacy::Secret).await;
    applied(
        add_organization_team_member(
            pool,
            f.owner.id,
            f.org,
            visible,
            f.member.id,
            TeamRole::Maintainer,
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            pool,
            f.owner.id,
            f.org,
            secret,
            f.member.id,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );

    // A visible team is for organization members. Anonymous viewers, non-members and site
    // administrators get the same 404 as a missing team.
    let path = format!("{TEAMS}/writers");
    let seen = f.get(&bystander, &path).await;
    assert_eq!(seen.status, StatusCode::OK);
    let page = team_page(&seen);
    assert_eq!(page.team.name, "writers");
    assert_eq!(page.team.slug, "writers");
    assert!(!page.manage);
    assert!(!page.show_repositories);
    assert_eq!(page.team.member_count, 1);
    assert_eq!(page.repository_count, 0);
    assert_eq!(page.members.len(), 1);
    assert_eq!(page.members[0].username, "ordinary_member");
    assert_eq!(page.members[0].role, "maintainer");
    // A team maintainer cannot manage the team, so the page offers no link.
    assert!(!team_page(&f.get(&f.member, &path).await).manage);
    assert!(team_page(&f.get(&f.owner, &path).await).manage);
    let raw = f
        .request(
            &bystander,
            "GET",
            &path,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert!(raw
        .body
        .contains("href=\"/orgs/native_org/teams/writers?tab=repositories\""));
    assert!(raw.body.contains("href=\"/ordinary_member\""));
    assert!(!raw.body.contains("Manage team"));
    for reply in [
        anonymous(f, &path, true).await,
        anonymous(f, &path, false).await,
        f.get(&stranger, &path).await,
        f.get(&f.outsider, &path).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    // A secret team answers like a missing one to everyone outside it. Its members, and the
    // organization's owners and admins, see it.
    let secret_path = format!("{TEAMS}/council");
    for reply in [
        anonymous(f, &secret_path, true).await,
        f.get(&stranger, &secret_path).await,
        f.get(&f.outsider, &secret_path).await,
        f.get(&bystander, &secret_path).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
    assert_eq!(f.get(&f.member, &secret_path).await.status, StatusCode::OK);
    let owner_view = f.get(&f.owner, &secret_path).await;
    assert_eq!(owner_view.status, StatusCode::OK);
    assert!(team_page(&owner_view).manage);

    // Unknown organizations, unknown teams and hidden teams answer the same page.
    let missing_org = f.get(&bystander, "/orgs/no_such_org/teams/writers").await;
    let missing_team = f.get(&bystander, &format!("{TEAMS}/no_such_team")).await;
    let hidden = f.get(&stranger, &secret_path).await;
    for reply in [&missing_org, &missing_team, &hidden] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
    assert_eq!(missing_org.error_message(), missing_team.error_message());
    assert_eq!(missing_team.error_message(), hidden.error_message());

    // Repository grants list only the repositories the viewer may read, before paging.
    let open = f
        .database
        .organization_repository(f.org, "open_repo", false)
        .await;
    let private = f
        .database
        .organization_repository(f.org, "hidden_repo", true)
        .await;
    for repo in [open, private] {
        applied(
            grant_organization_team_repository(
                pool,
                f.owner.id,
                f.org,
                visible,
                repo,
                TeamRepositoryPermission::new(3).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    let repositories = format!("{path}?tab=repositories");
    let listed = team_page(&f.get(&bystander, &repositories).await);
    assert!(listed.show_repositories);
    assert_eq!(repository_names(&listed), ["open_repo"]);
    assert_eq!(listed.repositories[0].owner, "native_org");
    assert_eq!(listed.repositories[0].permission, 3);
    assert_eq!(listed.repository_count, 1);
    // A team member reads the private repository through the team's own grant.
    assert_eq!(
        repository_names(&team_page(&f.get(&f.member, &repositories).await)),
        ["hidden_repo", "open_repo"]
    );
    let owner_repos = team_page(&f.get(&f.owner, &repositories).await);
    assert_eq!(repository_names(&owner_repos), ["hidden_repo", "open_repo"]);
    assert_eq!(owner_repos.repository_count, 2);
    let raw_repos = f
        .request(
            &bystander,
            "GET",
            &repositories,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert!(raw_repos.body.contains("href=\"/native_org/open_repo\""));
    assert!(!raw_repos.body.contains("hidden_repo"));

    // A full page links onward; the next page holds the rest and no further link.
    for index in 0..100 {
        let repo = f
            .database
            .organization_repository(f.org, &format!("bulk_{index:03}"), false)
            .await;
        applied(
            grant_organization_team_repository(
                pool,
                f.owner.id,
                f.org,
                visible,
                repo,
                TeamRepositoryPermission::new(1).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    let first = team_page(&f.get(&bystander, &repositories).await);
    assert_eq!(first.repositories.len(), 100);
    assert!(first.more);
    assert_eq!(first.repository_count, 101);
    let next = format!("{repositories}&after={}", first.after);
    let second = team_page(&f.get(&bystander, &next).await);
    assert_eq!(second.repositories.len(), 1);
    assert!(!second.more);
    let first_raw = f
        .request(
            &bystander,
            "GET",
            &repositories,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert!(first_raw.body.contains("class=\"org-pagination\""));
    assert!(first_raw.body.contains(&format!("after={}", first.after)));
    let second_raw = f
        .request(
            &bystander,
            "GET",
            &next,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert!(!second_raw.body.contains("class=\"org-pagination\""));

    // A malformed cursor is refused like the other organization lists.
    assert_eq!(
        f.get(&bystander, &format!("{path}?after=-1")).await.status,
        StatusCode::BAD_REQUEST
    );

    // The settings panel links to the team page and keeps its editing controls.
    let settings = f
        .request(
            &f.owner,
            "GET",
            &format!("{}?team={visible}", f.path()),
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(settings.status, StatusCode::OK);
    assert!(settings.body.contains("View team page"));
    assert!(settings
        .body
        .contains("href=\"/orgs/native_org/teams/writers\""));
    assert!(settings.body.contains("Save team"));
}
