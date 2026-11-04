use crate::organizations::properties::checked_changes;
use crate::organizations::properties::checked_definitions;
use crate::organizations::properties::DefinitionSummary;
use crate::organizations::properties::RepositoryMetadata;
use crate::organizations::properties::RepositoryValues;
use crate::organizations::properties::MAX_DEFINITIONS;
use crate::sqlite::organizations::authority::credential;
use crate::sqlite::organizations::authority::require_org;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Repository;
use codeyard_db_core::queries::organizations::properties::effective_values;
use codeyard_db_core::queries::organizations::properties::invalid;
use codeyard_db_core::queries::organizations::properties::Definition;
use codeyard_db_core::queries::organizations::properties::DefinitionInput;
use codeyard_db_core::queries::organizations::properties::DefinitionRow;
use codeyard_db_core::queries::organizations::properties::EditableBy;
use codeyard_db_core::queries::organizations::properties::PropertyValue;
use codeyard_db_core::queries::organizations::properties::Refusal;
use codeyard_db_core::queries::organizations::properties::ValueInput;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::sqlite::organizations::authority::begin;
use codeyard_db_core::queries::sqlite::organizations::authority::begin_read;
use codeyard_db_core::queries::sqlite::organizations::authority::read_authority;
use codeyard_db_core::queries::sqlite::organizations::authority::require_read_org;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use std::collections::HashSet;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Conflict;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;

pub async fn definitions_on(c: &mut SqliteConnection, org: i64) -> Result<Vec<Definition>> {
    sqlx::query_as::<_,DefinitionRow>("SELECT * FROM organization_repository_properties WHERE org_id=?1 ORDER BY LOWER(property_name),id").bind(org).fetch_all(c).await?.into_iter().map(DefinitionRow::decode).collect()
}
pub async fn catalog(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<Definition>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let result = definitions_on(&mut tx, org).await?;
    tx.commit().await?;
    Ok(Applied(result))
}
pub async fn summaries(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<OrganizationOutcome<Vec<DefinitionSummary>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let result = sqlx::query_as::<_, DefinitionSummary>("SELECT id,property_name,value_type,description,required,require_explicit_values FROM organization_repository_properties WHERE org_id=?1 ORDER BY LOWER(property_name),id LIMIT ?2")
        .bind(org)
        .bind(MAX_DEFINITIONS as i64)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Applied(result))
}

async fn compatible_on(
    c: &mut SqliteConnection,
    org: i64,
    old: Option<&Definition>,
    input: &DefinitionInput,
) -> Result<()> {
    if let Some(old) = old.filter(|old| !input.same_value_rules(&old.fields)) {
        let mut after = 0;
        loop {
            let rows:Vec<(i64,String)>=sqlx::query_as("SELECT repo_id,value FROM repository_property_values WHERE org_id=?1 AND definition_id=?2 AND repo_id>?3 ORDER BY repo_id LIMIT 500").bind(org).bind(old.id).bind(after).fetch_all(&mut *c).await?;
            if rows.is_empty() {
                break;
            }
            for (repo, value) in rows {
                input.validate_value(&serde_json::from_str::<PropertyValue>(&value)?)?;
                after = repo;
            }
        }
    }
    if input.needs_explicit_value() && old.is_none_or(|old| !old.fields.needs_explicit_value()) {
        let missing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.owner_kind='org' AND r.owner_org_id=?1 AND r.owner_user_id IS NULL AND NOT EXISTS(SELECT 1 FROM repository_property_values v WHERE v.repo_id=r.id AND v.org_id=?1 AND v.definition_id=?2))").bind(org).bind(old.map(|old|old.id)).fetch_one(c).await?;
        ensure!(
            !missing,
            invalid(
                "required",
                "Assign explicit values to existing repositories before requiring them"
            )
        );
    }
    Ok(())
}
pub async fn define(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    inputs: &[DefinitionInput],
) -> Result<OrganizationOutcome<Vec<Definition>>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "organization_custom_properties").await? {
        return Ok(Denied);
    }
    let existing = definitions_on(&mut tx, org).await?;
    checked_definitions(inputs, &existing)?;
    for input in inputs {
        let old = existing.iter().find(|old| {
            old.fields
                .property_name
                .eq_ignore_ascii_case(&input.property_name)
        });
        compatible_on(&mut tx, org, old, input).await?;
    }
    let mut result = Vec::new();
    for input in inputs {
        let row=sqlx::query_as::<_,DefinitionRow>("INSERT INTO organization_repository_properties(org_id,property_name,value_type,required,default_value,description,allowed_values,values_editable_by,require_explicit_values) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(org_id,LOWER(property_name)) DO UPDATE SET property_name=excluded.property_name,value_type=excluded.value_type,required=excluded.required,default_value=excluded.default_value,description=excluded.description,allowed_values=excluded.allowed_values,values_editable_by=excluded.values_editable_by,require_explicit_values=excluded.require_explicit_values RETURNING *")
            .bind(org).bind(&input.property_name).bind(input.value_type.as_str()).bind(input.required).bind(input.default_value.as_ref().map(serde_json::to_string).transpose()?).bind(&input.description).bind(input.allowed_values.as_ref().map(serde_json::to_string).transpose()?).bind(input.values_editable_by.as_str()).bind(input.require_explicit_values).fetch_one(&mut *tx).await?;
        result.push(row.decode()?);
    }
    tx.commit().await?;
    Ok(Applied(result))
}
pub async fn remove(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    name: &str,
    definition: Option<i64>,
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "organization_custom_properties").await? {
        return Ok(Denied);
    }
    // A native form names the definition it showed, so a replacement with the same name survives.
    if sqlx::query("DELETE FROM organization_repository_properties WHERE org_id=?1 AND LOWER(property_name)=LOWER(?2) AND (?3 IS NULL OR id=?3)").bind(org).bind(name).bind(definition).execute(&mut *tx).await?.rows_affected()==0 {
        return Ok(if definition.is_some() { Conflict } else { Missing });
    }
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn apply_on(
    c: &mut SqliteConnection,
    org: i64,
    repo: i64,
    changes: &[(i64, Option<String>)],
) -> Result<()> {
    for (definition, value) in changes {
        if let Some(value) = value {
            sqlx::query("INSERT INTO repository_property_values(org_id,repo_id,definition_id,value) VALUES(?1,?2,?3,?4) ON CONFLICT(repo_id,definition_id) DO UPDATE SET value=excluded.value").bind(org).bind(repo).bind(definition).bind(value).execute(&mut *c).await?;
        } else {
            sqlx::query("DELETE FROM repository_property_values WHERE org_id=?1 AND repo_id=?2 AND definition_id=?3").bind(org).bind(repo).bind(definition).execute(&mut *c).await?;
        }
    }
    Ok(())
}
pub async fn creation_values_on(
    c: &mut SqliteConnection,
    actor: i64,
    org: i64,
    inputs: &[ValueInput],
) -> Result<Vec<(i64, Option<String>)>> {
    let definitions = definitions_on(c, org).await?;
    let owner = matches!(
        read_authority(c, actor, org).await?,
        Applied(OrgRole::Owner)
    );
    for input in inputs {
        let definition = definitions
            .iter()
            .find(|definition| {
                definition
                    .fields
                    .property_name
                    .eq_ignore_ascii_case(&input.property_name)
            })
            .ok_or_else(|| invalid("property_name", "Unknown organization repository property"))?;
        ensure!(
            owner || definition.fields.values_editable_by == EditableBy::OrgAndRepoActors,
            Refusal::Denied
        );
    }
    let changes = checked_changes(&definitions, inputs)?;
    for definition in &definitions {
        if definition.fields.needs_explicit_value() {
            ensure!(
                changes
                    .iter()
                    .any(|(id, value)| *id == definition.id && value.is_some()),
                invalid(
                    "custom_properties",
                    "Provide the required organization repository properties"
                )
            );
        }
    }
    Ok(changes)
}
pub async fn set_values(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    repositories: &[String],
    values: &[ValueInput],
) -> Result<OrganizationOutcome<()>> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    if !credential(&mut tx, &grant, org, "organization_custom_properties").await? {
        return Ok(Denied);
    }
    ensure!(
        !repositories.is_empty() && repositories.len() <= 30,
        invalid(
            "repository_names",
            "Provide between 1 and 30 repository names"
        )
    );
    let definitions = definitions_on(&mut tx, org).await?;
    let changes = checked_changes(&definitions, values)?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    // Repository names are exact: `Docs` and `docs` are two repositories.
    for name in repositories {
        if !seen.insert(name) {
            continue;
        }
        let id:Option<i64>=sqlx::query_scalar("SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?1 AND owner_user_id IS NULL AND name=?2").bind(org).bind(name).fetch_optional(&mut *tx).await?;
        ids.push(id.ok_or_else(|| {
            invalid(
                "repository_names",
                "Every repository must belong to this organization",
            )
        })?);
    }
    ids.sort_unstable();
    for repo in ids {
        apply_on(&mut tx, org, repo, &changes).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}

pub async fn repository(
    pool: &SqlitePool,
    viewer: Option<i64>,
    repo: i64,
) -> Result<Option<RepositoryMetadata>> {
    let mut tx = begin_read(pool).await?;
    let Some(repository)=sqlx::query_as::<_,Repository>("SELECT * FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL").bind(repo).fetch_optional(&mut *tx).await? else {return Ok(None);};
    let access = if let Some(viewer) = viewer {
        codeyard_db_core::queries::sqlite::repositories::access::user_repo_access_in_transaction(
            &mut tx, viewer, repo,
        )
        .await?
    } else {
        AccessLevel::None
    };
    if repository.is_private && !access.can_read() {
        return Ok(None);
    }
    let org = repository
        .owner_org_id
        .expect("validated organization repository");
    let organization_owner = if let Some(viewer) = viewer {
        access.can_admin()
            && matches!(
                read_authority(&mut tx, viewer, org).await?,
                Applied(OrgRole::Owner)
            )
    } else {
        false
    };
    let definitions = definitions_on(&mut tx, org).await?;
    let properties = values_on(&mut tx, repo, &definitions).await?;
    tx.commit().await?;
    Ok(Some(RepositoryMetadata {
        values: RepositoryValues {
            repository,
            properties,
        },
        definitions,
        organization_owner,
        repository_admin: access.can_admin(),
    }))
}
pub async fn set_repository_values(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    values: &[ValueInput],
) -> Result<()> {
    change_repository_values(pool, actor, repo, values, None).await
}
/// Native forms bind the definition's immutable identity as well as the repository.
pub async fn set_repository_value(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    definition: i64,
    value: &ValueInput,
) -> Result<()> {
    change_repository_values(
        pool,
        actor,
        repo,
        std::slice::from_ref(value),
        Some(definition),
    )
    .await
}
async fn change_repository_values(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    values: &[ValueInput],
    expected: Option<i64>,
) -> Result<()> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = begin(pool).await?;
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_archive_authority(
            &mut tx, actor, repo
        )
        .await?,
        Refusal::Denied
    );
    let org:i64=sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL").bind(repo).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    let mut owner = matches!(
        read_authority(&mut tx, actor, org).await?,
        Applied(OrgRole::Owner)
    );
    ensure!(
        codeyard_db_core::queries::sqlite::credentials::writes(
            &mut tx,
            &grant,
            Some(repo),
            None,
            "administration"
        )
        .await?,
        Refusal::Denied
    );
    // Account ownership cannot lend an organization-only capability to a repository token.
    if owner && grant.token_id().is_some() {
        owner = codeyard_db_core::queries::sqlite::credentials::writes(
            &mut tx,
            &grant,
            None,
            Some(org),
            "organization_custom_properties",
        )
        .await?;
    }
    let definitions = definitions_on(&mut tx, org).await?;
    if let Some(id) = expected {
        ensure!(
            values.len() == 1
                && definitions.iter().any(|definition| definition.id == id
                    && definition
                        .fields
                        .property_name
                        .eq_ignore_ascii_case(&values[0].property_name)),
            Refusal::Missing
        );
    }
    for input in values {
        let definition = definitions
            .iter()
            .find(|definition| {
                definition
                    .fields
                    .property_name
                    .eq_ignore_ascii_case(&input.property_name)
            })
            .ok_or_else(|| invalid("property_name", "Unknown organization repository property"))?;
        ensure!(
            owner || definition.fields.values_editable_by == EditableBy::OrgAndRepoActors,
            Refusal::Denied
        );
    }
    let changes = checked_changes(&definitions, values)?;
    apply_on(&mut tx, org, repo, &changes).await?;
    tx.commit().await?;
    Ok(())
}

fn query_predicates(
    builder: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>,
    query: &query::RepositoryQuery,
) {
    use query::Kind;
    if query.groups.is_empty() {
        return;
    }
    builder.push(" AND (");
    for (index, group) in query.groups.iter().enumerate() {
        if index > 0 {
            builder.push(" OR ");
        }
        builder.push("(");
        for (index, criterion) in group.iter().enumerate() {
            if index > 0 {
                builder.push(" AND ");
            }
            builder.push(if criterion.negative { "NOT(" } else { "(" });
            match &criterion.kind {
                Kind::Text(value) => {
                    let needle = codeyard_db_core::queries::sqlite::search::contains(value);
                    builder
                        .push("r.name REGEXP ")
                        .push_bind(needle.clone())
                        .push(" OR COALESCE(r.description,'') REGEXP ")
                        .push_bind(needle.clone())
                        .push(" OR o.name REGEXP ")
                        .push_bind(needle);
                }
                Kind::Name(value) => {
                    builder
                        .push("r.name REGEXP ")
                        .push_bind(codeyard_db_core::queries::sqlite::search::contains(value));
                }
                Kind::Owner(value) => {
                    builder
                        .push("LOWER(o.name)=LOWER(")
                        .push_bind(value.clone())
                        .push(")");
                }
                Kind::Repository(owner, name) => {
                    builder
                        .push("LOWER(o.name)=LOWER(")
                        .push_bind(owner.clone())
                        .push(") AND LOWER(r.name)=LOWER(")
                        .push_bind(name.clone())
                        .push(")");
                }
                Kind::Private(value) => {
                    builder.push("r.is_private=").push_bind(*value);
                }
                Kind::Archived(value) => {
                    builder.push("r.is_archived=").push_bind(*value);
                }
                Kind::Fork(value) => {
                    builder.push("r.is_fork=").push_bind(*value);
                }
                Kind::Language(value) => {
                    builder.push("COALESCE(LOWER((SELECT l.language FROM repo_languages l WHERE l.repo_id=r.id ORDER BY l.bytes DESC,l.language LIMIT 1)),'')=LOWER(").push_bind(value.clone()).push(")");
                }
                Kind::Topic(value) => {
                    builder.push("EXISTS(SELECT 1 FROM repository_topics t WHERE t.repo_id=r.id AND LOWER(t.topic)=LOWER(").push_bind(value.clone()).push("))");
                }
                Kind::Property(name, value) => {
                    builder.push("EXISTS(SELECT 1 FROM organization_repository_properties d LEFT JOIN repository_property_values v ON v.definition_id=d.id AND v.repo_id=r.id AND v.org_id=d.org_id JOIN json_each(COALESCE(CASE WHEN d.require_explicit_values THEN v.value ELSE COALESCE(v.value,d.default_value) END,'null')) selected WHERE d.org_id=r.owner_org_id AND LOWER(d.property_name)=LOWER(").push_bind(name.clone()).push(") AND selected.value=").push_bind(value.clone()).push(")");
                }
                Kind::Any => {
                    builder.push("TRUE");
                }
                Kind::Never => {
                    builder.push("FALSE");
                }
            }
            builder.push(")");
        }
        builder.push(")");
    }
    builder.push(")");
}
pub async fn list_values(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    query: &query::RepositoryQuery,
    window: (i64, i64),
) -> Result<OrganizationOutcome<Vec<RepositoryValues>>> {
    ensure!(
        window.0 >= 0 && (1..=101).contains(&window.1),
        invalid("page", "Invalid repository property page")
    );
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let definitions = definitions_on(&mut tx, org).await?;
    for criterion in query.groups.iter().flatten() {
        if let query::Kind::Property(name, _) = &criterion.kind {
            ensure!(
                definitions
                    .iter()
                    .any(|definition| definition.fields.property_name.eq_ignore_ascii_case(name)),
                invalid(
                    "repository_query",
                    "Unknown organization repository property"
                )
            );
        }
    }
    let mut builder=sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT r.* FROM repositories r JOIN organizations o ON o.id=r.owner_org_id JOIN users u ON u.id=");
    builder
        .push_bind(actor)
        .push(" WHERE r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=")
        .push_bind(org)
        .push(format!(
            " AND (NOT r.is_private OR ({})>=1)",
            codeyard_db_core::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL
        ));
    query_predicates(&mut builder, query);
    builder
        .push(" ORDER BY r.id LIMIT ")
        .push_bind(window.1)
        .push(" OFFSET ")
        .push_bind(window.0);
    let repositories = builder
        .build_query_as::<Repository>()
        .fetch_all(&mut *tx)
        .await?;
    let ids: Vec<i64> = repositories
        .iter()
        .map(|repository| repository.id)
        .collect();
    let rows: Vec<(i64, i64, String)> = sqlx::query_as("SELECT repo_id,definition_id,value FROM repository_property_values WHERE repo_id IN (SELECT value FROM json_each(?1))")
        .bind(serde_json::to_string(&ids)?).fetch_all(&mut *tx).await?;
    let mut values: std::collections::HashMap<i64, std::collections::HashMap<i64, String>> =
        std::collections::HashMap::new();
    for (repo, definition, value) in rows {
        values.entry(repo).or_default().insert(definition, value);
    }
    let mut result = Vec::new();
    let mut bytes = 0;
    for repository in repositories {
        let properties = codeyard_db_core::queries::organizations::properties::effective_values(
            &definitions,
            &values.remove(&repository.id).unwrap_or_default(),
        )?;
        for property in &properties {
            bytes +=
                property.property_name.len() + serde_json::to_string(&property.value)?.len() + 64;
        }
        bytes += repository.name.len() + 128;
        ensure!(bytes <= 64 * 1024 * 1024, Refusal::TooLarge);
        result.push(RepositoryValues {
            repository,
            properties,
        });
    }
    tx.commit().await?;
    Ok(Applied(result))
}

use codeyard_db_core::queries::sqlite::organizations::properties::definition;

use codeyard_db_core::queries::sqlite::organizations::properties::values_on;

use codeyard_db_core::queries::sqlite::organizations::properties::definition;

use codeyard_db_core::queries::sqlite::organizations::properties::values_on;
