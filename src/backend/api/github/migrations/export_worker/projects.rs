use super::*;

pub(super) async fn append(
    state: &AppState,
    actor: i64,
    row: &RepositoryExport,
    archive: &mut MigrationArchive,
) -> Result<()> {
    let (owner, name) = if let Some(org) = row.owner_org_id {
        let org = db::organizations::get_public_organization_by_id(&state.pool, org)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        (db::projects::Owner::Organization(org.id), org.name)
    } else {
        let id = row.owner_user_id.ok_or_else(Error::missing)?;
        let user = db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        (db::projects::Owner::User(id), user.username)
    };
    let mut after = 0;
    loop {
        let rows = db::projects::list_page(&state.pool, Some(actor), owner, after, 100)
            .await
            .map_err(Error::unavailable)?;
        let count = rows.len();
        let mut values = Vec::new();
        for project in rows {
            after = project.id;
            let url = crate::backend::api::github::projects::project_url(&project, &name);
            let mut value =
                crate::backend::api::github::projects::document(state, &project, &name).await?;
            value["url"] = json!(url);
            value["created_at"] = json!(project.created_at);
            value["updated_at"] = json!(project.updated_at);
            value["closed_at"] = json!(project.closed_at);
            values.push(value);
            let mut field_after = 0;
            loop {
                let fields = db::projects::fields_page(
                    &state.pool,
                    Some(actor),
                    project.id,
                    field_after,
                    100,
                )
                .await
                .map_err(Error::unavailable)?;
                let count = fields.len();
                let mut values = Vec::new();
                for field in fields {
                    field_after = field.id;
                    let mut value =
                        payloads::project_field(&field, &url).map_err(Error::unavailable)?;
                    value["project"] = json!(url);
                    value["created_at"] = json!(field.created_at);
                    value["updated_at"] = json!(field.updated_at);
                    // Preserve raw configuration as well as its API projection (including option ids).
                    value["source_configuration"] =
                        db::projects::field_configuration(&field).map_err(Error::unavailable)?;
                    values.push(value);
                }
                archive
                    .records("project_field", values)
                    .map_err(Error::unavailable)?;
                if count < 100 {
                    break;
                }
            }
            // Items keep their content links, draft text, field values, order and archive
            // state; views keep their saved configuration. Items in repositories the exporter
            // cannot read are left out, as the project page leaves them out.
            let mut values = Vec::new();
            for item in db::projects::items(&state.pool, Some(actor), project.id)
                .await
                .map_err(Error::unavailable)?
            {
                let mut value = serde_json::to_value(&item).map_err(Error::unavailable)?;
                value["project"] = json!(url);
                value["content_type"] = json!(item.content_type());
                value["field_values"] =
                    serde_json::from_str(&item.field_values).map_err(Error::unavailable)?;
                values.push(value);
            }
            archive
                .records("project_item", values)
                .map_err(Error::unavailable)?;
            let mut values = Vec::new();
            for view in db::projects::views(&state.pool, Some(actor), project.id)
                .await
                .map_err(Error::unavailable)?
            {
                let mut value = serde_json::to_value(&view).map_err(Error::unavailable)?;
                value["project"] = json!(url);
                value["configuration"] =
                    serde_json::from_str(&view.configuration).map_err(Error::unavailable)?;
                values.push(value);
            }
            archive
                .records("project_view", values)
                .map_err(Error::unavailable)?;
        }
        archive
            .records("project", values)
            .map_err(Error::unavailable)?;
        if count < 100 {
            break;
        }
    }
    Ok(())
}
