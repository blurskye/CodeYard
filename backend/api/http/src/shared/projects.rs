//! Project pages and plain forms use the same project operations as REST.
use crate::context::PageContext;
use codeyard_db_repos::projects::Content;
use codeyard_db_repos::projects::Owner;
use codeyard_webui::pages::projects as view;
use std::collections::HashMap;
pub type Params = HashMap<String, String>;

pub async fn sidebar(
    ctx: &PageContext,
    repo: i64,
    kind: codeyard_webui::pages::repos::issues::Kind,
    id: i64,
) -> anyhow::Result<Vec<view::Assignment>> {
    let pool = &ctx.state.pool;
    let viewer = ctx.user.as_ref().map(|u| u.id);
    let content = if kind == codeyard_webui::pages::repos::issues::Kind::Issue {
        Content::Issue(id)
    } else {
        Content::PullRequest(id)
    };
    let mut item_ids = HashMap::new();
    let mut offset = 0;
    loop {
        let (count, items) = codeyard_db_repos::projects::content_items(
            pool, viewer, &content, true, offset, 100, true,
        )
        .await?;
        for item in items {
            item_ids.insert(item.project_id, item.id);
        }
        offset += 100;
        if offset >= count {
            break;
        }
    }
    let mut assigned = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for filter in [
        codeyard_db_repos::projects::ListFilter {
            content: Some((matches!(content, Content::Issue(_)), id)),
            ..Default::default()
        },
        codeyard_db_repos::projects::ListFilter {
            repo: Some(repo),
            ..Default::default()
        },
    ] {
        let mut offset = 0;
        loop {
            let (count, rows) =
                codeyard_db_repos::projects::list(pool, viewer, &filter, offset, 100).await?;
            for project in rows {
                if seen.insert(project.id) {
                    assigned.push(project);
                }
            }
            offset += 100;
            if offset >= count {
                break;
            }
        }
    }
    let mut result = Vec::new();
    for project in assigned {
        let owner = if project.owner_kind == "org" {
            Owner::Organization(project.owner_id)
        } else {
            Owner::User(project.owner_id)
        };
        let can_edit = match viewer {
            Some(id) => codeyard_db_repos::projects::can_write(pool, id, owner).await?,
            None => false,
        };
        let item_id = item_ids.get(&project.id).copied();
        if item_id.is_none() && !can_edit {
            continue;
        }
        let name = match owner {
            Owner::User(id) => {
                codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
                    .await?
                    .ok_or(codeyard_model::project_query::Refusal::Missing)?
                    .username
            }
            Owner::Organization(id) => {
                codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(pool, id)
                    .await?
                    .ok_or(codeyard_model::project_query::Refusal::Missing)?
                    .name
            }
        };
        let path = format!(
            "/{}/{}/projects/{}",
            if project.owner_kind == "org" {
                "orgs"
            } else {
                "users"
            },
            codeyard_types::paths::encode_component(&name),
            project.number
        );
        result.push(view::Assignment {
            path,
            title: project.title,
            item_id,
            can_edit,
        });
    }
    Ok(result)
}

pub async fn owner(ctx: &PageContext, params: &Params) -> anyhow::Result<(Owner, String, String)> {
    if let Some(name) = params.get("org") {
        let org = codeyard_db_orgs::organizations::profiles::get_public_organization(
            &ctx.state.pool,
            name,
        )
        .await?
        .ok_or(codeyard_model::project_query::Refusal::Missing)?;
        let base = format!(
            "/orgs/{}/projects",
            codeyard_types::paths::encode_component(&org.name)
        );
        Ok((Owner::Organization(org.id), org.name, base))
    } else {
        let name = params
            .get("username")
            .ok_or(codeyard_model::project_query::Refusal::Missing)?;
        let user =
            codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, name)
                .await?
                .ok_or(codeyard_model::project_query::Refusal::Missing)?;
        let base = format!(
            "/users/{}/projects",
            codeyard_types::paths::encode_component(&user.username)
        );
        Ok((Owner::User(user.id), user.username, base))
    }
}
