//! The public organization page at /NAME: each viewer sees the repositories the store allows.
use crate::fixture::{Actor, Fixture, Reply};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::{
    backend::db::organizations::TeamPrivacy,
    webui::{app::Page, bootstrap, pages::organizations::OrganizationProfile},
};
use std::collections::HashSet;
use tower::ServiceExt;

async fn anonymous_get(f: &Fixture, path: &str) -> Reply {
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::ACCEPT, bootstrap::MODEL_CONTENT_TYPE)
                .body(Body::empty())
                .unwrap(),
        )
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

fn public_profile(reply: &Reply) -> OrganizationProfile {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    match reply.envelope().page {
        Page::OrganizationProfile(profile) => profile,
        other => panic!("expected organization profile, got {other:?}"),
    }
}

fn listed(profile: &OrganizationProfile) -> Vec<&str> {
    profile
        .repositories
        .iter()
        .map(|repo| repo.name.as_str())
        .collect()
}

/// The repositories this test creates. The pagination contract adds public ones of its own.
fn ours(profile: &OrganizationProfile) -> Vec<&str> {
    listed(profile)
        .into_iter()
        .filter(|name| *name == "open-tools" || *name == "closed-tools")
        .collect()
}

pub async fn contracts(f: &Fixture) {
    f.database
        .organization_repository(f.org, "open-tools", false)
        .await;
    f.database
        .organization_repository(f.org, "closed-tools", true)
        .await;

    let anonymous = public_profile(&anonymous_get(f, "/native_org").await);
    assert_eq!(anonymous.organization.name, "native_org");
    assert_eq!(ours(&anonymous), ["open-tools"]);
    assert!(!anonymous.manage);
    // The member count is for readers of the organization only.
    assert_eq!(anonymous.member_count, None);

    // Owners see every repository, in name order, and the member count.
    let members = f
        .database
        .count(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1",
            f.org,
        )
        .await;
    let owner = public_profile(&f.get(&f.owner, "/native_org").await);
    assert_eq!(ours(&owner), ["closed-tools", "open-tools"]);
    assert!(owner.manage);
    assert_eq!(owner.member_count, Some(members));

    // An ordinary member has no read grant on the private repository and cannot manage the
    // organization, but may see its member count.
    let member = public_profile(&f.get(&f.member, "/native_org").await);
    assert_eq!(ours(&member), ["open-tools"]);
    assert!(!member.manage);
    assert_eq!(member.member_count, Some(members));

    // A site administrator who is not a member reads the public page only.
    let outsider = public_profile(&f.get(&f.outsider, "/native_org").await);
    assert_eq!(ours(&outsider), ["open-tools"]);
    assert!(!outsider.manage);
    assert_eq!(outsider.member_count, None);

    // An unknown organization answers exactly like an unknown user.
    let missing_org = f.get(&f.outsider, "/no-such-name").await;
    let missing_user = f.get(&f.outsider, "/no-such-user").await;
    assert_eq!(missing_org.status, StatusCode::NOT_FOUND);
    assert_eq!(missing_org.status, missing_user.status);
    assert_eq!(missing_org.error_message(), missing_user.error_message());
    assert_eq!(
        missing_org.error_message(),
        "No page, user or organization has this name."
    );
}

/// Every page of one member tab, followed through the next-page cursors.
async fn every_page(f: &Fixture, actor: &Actor, tab: &str) -> Vec<OrganizationProfile> {
    let mut pages: Vec<OrganizationProfile> = Vec::new();
    let mut after = 0;
    loop {
        let path = format!("/native_org?tab={tab}&after={after}");
        let page = public_profile(&f.get(actor, &path).await);
        after = page.after;
        let more = page.more;
        pages.push(page);
        assert!(pages.len() < 50, "the cursor must move forward");
        if !more {
            return pages;
        }
    }
}

/// The People and Teams tabs: members read them page by page, everyone else gets repositories.
/// Runs after the pagination contract, which leaves more than one page of members.
pub async fn tabs(f: &Fixture) {
    let members = f
        .database
        .count(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1",
            f.org,
        )
        .await;
    f.team("tab_visible", TeamPrivacy::Visible).await;
    f.team("tab_secret", TeamPrivacy::Secret).await;

    // The owner reads every member once, in pages of 30 with a cursor for each next page.
    let pages = every_page(f, &f.owner, "people").await;
    assert!(pages.len() > 1);
    assert!(pages
        .iter()
        .all(|page| page.tab == "people" && page.member_count == Some(members)));
    let (last, full) = pages.split_last().unwrap();
    assert!(full.iter().all(|page| page.people.len() == 30));
    assert!(last.people.len() <= 30 && !last.more);
    let ids: Vec<i64> = pages
        .iter()
        .flat_map(|page| page.people.iter().map(|member| member.user_id))
        .collect();
    assert_eq!(ids.len() as i64, members);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());

    // Secret teams reach the owner, and an ordinary member sees only the visible ones.
    let owner_teams: Vec<String> = every_page(f, &f.owner, "teams")
        .await
        .iter()
        .flat_map(|page| page.teams.iter().map(|team| team.slug.clone()))
        .collect();
    assert!(owner_teams.contains(&"tab_secret".to_owned()));
    let member_teams: Vec<String> = every_page(f, &f.member, "teams")
        .await
        .iter()
        .flat_map(|page| page.teams.iter().map(|team| team.slug.clone()))
        .collect();
    assert!(member_teams.contains(&"tab_visible".to_owned()));
    assert!(!member_teams.contains(&"tab_secret".to_owned()));

    // The HTML a member gets carries the tab strip, and the People tab is selected.
    let member_html = f
        .request(
            &f.member,
            "GET",
            "/native_org?tab=people",
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(member_html.status, StatusCode::OK);
    assert!(member_html.body.contains("href=\"/native_org?tab=teams\""));
    assert!(member_html.body.contains("ordinary_member"));

    // A visitor cannot read People, Teams or any member, whatever the tab name says.
    for path in [
        "/native_org?tab=people",
        "/native_org?tab=teams",
        "/native_org?tab=people&after=garbage",
    ] {
        let visitor = public_profile(&anonymous_get(f, path).await);
        assert_eq!(visitor.tab, "repositories");
        assert!(visitor.people.is_empty() && visitor.teams.is_empty());
        assert_eq!(visitor.member_count, None);
        assert!(!visitor.more);
    }
    // A site administrator who is not a member is a visitor here too.
    let outsider = public_profile(&f.get(&f.outsider, "/native_org?tab=people").await);
    assert_eq!(outsider.tab, "repositories");
    assert!(outsider.people.is_empty());
    let outsider_html = f
        .request(
            &f.outsider,
            "GET",
            "/native_org?tab=people",
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(outsider_html.status, StatusCode::OK);
    assert!(!outsider_html
        .body
        .contains("href=\"/native_org?tab=people\""));
    assert!(!outsider_html
        .body
        .contains("href=\"/native_org?tab=teams\""));
    assert!(!outsider_html.body.contains("ordinary_member"));

    // A malformed cursor is refused for the member tabs, which read it.
    for path in [
        "/native_org?tab=people&after=garbage",
        "/native_org?tab=teams&after=-1",
    ] {
        assert_eq!(f.get(&f.member, path).await.status, StatusCode::BAD_REQUEST);
    }
}
