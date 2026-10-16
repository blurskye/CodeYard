use crate::{
    fixture::{applied, Fixture, INDEX},
    profile::html,
};
use axum::http::StatusCode;
use codeyard::backend::db::{models::OrgRole, organizations::*};

pub async fn contracts(f: &Fixture) {
    for value in ["garbage", "-1", "9223372036854775808", "", "1.5"] {
        assert_eq!(
            f.get(&f.owner, &format!("{INDEX}?after={value}"))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        for key in ["after", "members_after", "teams_after"] {
            assert_eq!(
                f.get(&f.owner, &format!("{}?{key}={value}", f.path()))
                    .await
                    .status,
                StatusCode::BAD_REQUEST
            );
        }
    }
    // DAO visibility must filter before LIMIT: 101 hidden teams cannot starve
    // two visible teams later in the same ID range for an ordinary member.
    for index in 0..101 {
        f.team(&format!("hidden_{index:03}"), TeamPrivacy::Secret)
            .await;
    }
    let first_team = f.team("visible_first", TeamPrivacy::Visible).await;
    let second_team = f.team("visible_second", TeamPrivacy::Visible).await;
    for index in 0..101 {
        let actor = Fixture::actor(f.database.pool(), &format!("paged_member_{index:03}")).await;
        applied(
            add_organization_member(
                f.database.pool(),
                f.owner.id,
                f.org,
                actor.id,
                OrgRole::Member,
            )
            .await
            .unwrap(),
        );
    }
    let page = f.get(&f.member, &f.path()).await.organization();
    assert_eq!(page.members.len(), 100);
    assert_eq!(
        page.teams.iter().map(|team| team.id).collect::<Vec<_>>(),
        vec![first_team, second_team]
    );
    assert_eq!(page.teams_after, second_team);
    assert_ne!(
        page.members_after, page.teams_after,
        "fixture must actually separate the cursor spaces"
    );
    let source = html(page.clone());
    assert!(
        source.contains(&format!(
            "members_after={}&amp;teams_after=0",
            page.members_after
        )) || source.contains(&format!(
            "members_after={}&#38;teams_after=0",
            page.members_after
        ))
    );
    // Only two teams are visible, so the team list is not a full page and offers no next link.
    assert!(!source.contains(&format!("teams_after={second_team}")));
    let next_members = f
        .get(
            &f.member,
            &format!(
                "{}?members_after={}&teams_after=0",
                f.path(),
                page.members_after
            ),
        )
        .await
        .organization();
    assert_eq!(next_members.members.len(), 3);
    assert_eq!(
        next_members.teams, page.teams,
        "member paging must not advance teams"
    );
    let next_teams = f
        .get(
            &f.member,
            &format!("{}?members_after=0&teams_after={first_team}", f.path()),
        )
        .await
        .organization();
    assert_eq!(
        next_teams.members, page.members,
        "team paging must not advance members"
    );
    assert_eq!(
        next_teams
            .teams
            .iter()
            .map(|team| team.id)
            .collect::<Vec<_>>(),
        vec![second_team]
    );
    let source = html(next_teams);
    assert!(
        source.contains(&format!(
            "members_after={}&amp;teams_after={first_team}",
            page.members_after
        )) || source.contains(&format!(
            "members_after={}&#38;teams_after={first_team}",
            page.members_after
        ))
    );

    let team = f.team("detail_pagination", TeamPrivacy::Visible).await;
    let pool = f.database.pool();
    for index in 0..101 {
        let actor = codeyard::backend::db::users::accounts::get_user_by_username(
            pool,
            &format!("paged_member_{index:03}"),
        )
        .await
        .unwrap()
        .unwrap();
        applied(
            add_organization_team_member(pool, f.owner.id, f.org, team, actor.id, TeamRole::Member)
                .await
                .unwrap(),
        );
    }
    // Invalid legacy team membership rows must be filtered before LIMIT.
    f.database
        .execute(
            "UPDATE team_memberships SET role='unsupported' WHERE team_id=$1",
            &[team],
        )
        .await;
    // Legacy cross-org team membership must not become a tenant member read.
    f.database
        .execute(
            &format!(
                "INSERT INTO team_memberships(team_id,user_id,role) VALUES({team},{},'member')",
                f.outsider.id
            ),
            &[],
        )
        .await;
    for actor in [&f.owner, &f.member] {
        applied(
            add_organization_team_member(pool, f.owner.id, f.org, team, actor.id, TeamRole::Member)
                .await
                .unwrap(),
        );
    }
    // Use a separate ordinary observer: the member above legitimately reads
    // private grants through team membership, which is not the filtering case.
    let observer = Fixture::actor(pool, "detail_observer").await;
    applied(
        add_organization_member(pool, f.owner.id, f.org, observer.id, OrgRole::Member)
            .await
            .unwrap(),
    );
    let foreign_org = applied(
        create_organization(
            pool,
            f.owner.id,
            &NewOrganization::new(
                "paging_foreign",
                OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let foreign_repo = f
        .database
        .organization_repository(foreign_org, "foreign_paging_grant", false)
        .await;
    f.database.execute(&format!("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES({team},{foreign_repo},1)"), &[]).await;
    for index in 0..101 {
        let repo = f
            .database
            .organization_repository(f.org, &format!("hidden_grant_{index:03}"), true)
            .await;
        applied(
            grant_organization_team_repository(
                pool,
                f.owner.id,
                f.org,
                team,
                repo,
                TeamRepositoryPermission::new(1).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    let first_repo = f
        .database
        .organization_repository(f.org, "paged_public_first", false)
        .await;
    let second_repo = f
        .database
        .organization_repository(f.org, "paged_public_second", false)
        .await;
    let mut grant_ids = vec![];
    for repo in [first_repo, second_repo] {
        grant_ids.push(
            applied(
                grant_organization_team_repository(
                    pool,
                    f.owner.id,
                    f.org,
                    team,
                    repo,
                    TeamRepositoryPermission::new(1).unwrap(),
                )
                .await
                .unwrap(),
            )
            .id,
        );
    }
    let path = format!("{}?members_after=0&teams_after={first_team}&team={team}&team_members_after=0&team_repositories_after=0", f.path());
    for key in ["team", "team_members_after", "team_repositories_after"] {
        for value in ["bad", "-1", "9223372036854775808", "", "1.5"] {
            let query = if key == "team" {
                format!("team={value}")
            } else {
                format!("team={team}&{key}={value}")
            };
            assert_eq!(
                f.get(&observer, &format!("{}?{query}", f.path()))
                    .await
                    .status,
                StatusCode::BAD_REQUEST
            );
        }
    }
    let detail_page = f.get(&observer, &path).await.organization();
    let selected = detail_page.selected_team.as_ref().unwrap();
    assert_eq!(
        selected.members.len(),
        2,
        "101 malformed memberships cannot starve active known members"
    );
    assert_eq!(
        selected
            .repositories
            .iter()
            .map(|value| value.repo_id)
            .collect::<Vec<_>>(),
        vec![first_repo, second_repo],
        "101 private grants cannot starve readable grants"
    );
    assert_eq!(selected.repositories_after, grant_ids[1]);
    let member_cursor = f
        .database
        .count(
            &format!(
                "SELECT id FROM team_memberships WHERE team_id=$1 AND user_id={}",
                f.owner.id
            ),
            team,
        )
        .await;
    let next_members = f.get(&observer, &format!("{}?members_after={}&teams_after={first_team}&team={team}&team_members_after={member_cursor}&team_repositories_after=0", f.path(), detail_page.members_after)).await.organization();
    assert_eq!(
        next_members.members.len(),
        4,
        "only the organization-members cursor advances that list"
    );
    let selected_members = next_members.selected_team.as_ref().unwrap();
    assert_eq!(selected_members.members.len(), 1);
    assert_eq!(selected_members.members[0].user_id, f.member.id);
    assert_eq!(selected_members.repositories, selected.repositories);
    let next_repos = f.get(&observer, &format!("{}?members_after=0&teams_after={first_team}&team={team}&team_members_after=0&team_repositories_after={}", f.path(), grant_ids[0])).await.organization();
    assert_eq!(next_repos.members, detail_page.members);
    let selected_repos = next_repos.selected_team.as_ref().unwrap();
    assert_eq!(selected_repos.members, selected.members);
    assert_eq!(
        selected_repos
            .repositories
            .iter()
            .map(|value| value.repo_id)
            .collect::<Vec<_>>(),
        vec![second_repo]
    );
    // A short list of grants has no next page to link to.
    assert!(!html(next_repos).contains(">Next team repositories<"));
}
