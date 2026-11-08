//! `Repository.rulesets` and `Organization.rulesets` read the rule documents the REST API stores.

use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::rulesets::records::ListFilter;
use codeyard_database::queries::rulesets::records::Owner;
use codeyard_database::queries::rulesets::records::Page;
use codeyard_database::queries::rulesets::records::Ruleset;
use codeyard_database::queries::rulesets::Target;
use codeyard_types::node_id;
use serde_json::json;
use serde_json::Value;

fn upper(value: impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_value(value)
        .map_err(unavailable)?
        .as_str()
        .ok_or_else(|| unavailable(()))?
        .to_ascii_uppercase())
}

fn filter(ctx: &Arguments) -> Result<ListFilter> {
    let includes_parents = match ctx.args.get("includeParents") {
        Some(value) if !value.is_null() => value.deserialize()?,
        _ => true,
    };
    let targets = match ctx.args.get("targets") {
        Some(value) if !value.is_null() => Some(
            value
                .deserialize::<Vec<String>>()?
                .iter()
                .filter_map(|target| match target.as_str() {
                    "BRANCH" => Some(Target::Branch),
                    "TAG" => Some(Target::Tag),
                    "PUSH" => Some(Target::Push),
                    // The repository target has no native policy type.
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    };
    Ok(ListFilter {
        includes_parents,
        targets,
    })
}

pub(super) async fn list(
    ty: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let owner = if ty == "Organization" {
        request.authorize_organization(id).await?;
        Owner::Organization(id)
    } else {
        Owner::Repository(id)
    };
    let filter = filter(ctx)?;
    let mut rows = Vec::new();
    loop {
        let page = Page::new(rows.len() as i64, 100).map_err(unavailable)?;
        let found = match codeyard_db_repos::rulesets::operations::list_rulesets(
            &request.state.pool,
            Some(request.auth.user.id),
            owner,
            &filter,
            page,
        )
        .await
        .map_err(unavailable)?
        {
            OrganizationOutcome::Applied(found) => found,
            OrganizationOutcome::Denied => return Err(Error::new("Must have organization access")),
            _ => return Err(missing()),
        };
        let done = found.len() < 100;
        rows.extend(found);
        if done {
            break;
        }
    }
    let mut nodes = Vec::with_capacity(rows.len());
    for row in rows {
        let ruleset = row.value;
        nodes.push(object(ruleset, request).await?);
    }
    crate::connection::slice(ctx, nodes)
}

pub(super) async fn node(id: i64, request: &RequestContext) -> Result<Value> {
    let (repo, org) = codeyard_db_repos::graphql::ruleset_owner(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let owner = match (repo, org) {
        (Some(repo), None) => {
            request.authorize(repo, "metadata", false).await?;
            Owner::Repository(repo)
        }
        (None, Some(org)) => {
            request.authorize_organization(org).await?;
            Owner::Organization(org)
        }
        _ => return Err(missing()),
    };
    match codeyard_db_repos::rulesets::operations::get_ruleset(
        &request.state.pool,
        Some(request.auth.user.id),
        owner,
        id,
        false,
    )
    .await
    .map_err(unavailable)?
    {
        OrganizationOutcome::Applied(row) => object(row.value, request).await,
        _ => Err(missing()),
    }
}

async fn object(ruleset: Ruleset, request: &RequestContext) -> Result<Value> {
    let source = match ruleset.owner {
        Owner::Organization(org) => {
            request.authorize_organization(org).await?;
            crate::user::organization_by_id(org, request).await?
        }
        Owner::Repository(repo) => {
            crate::repository::object(&crate::repository::by_id(repo, request).await?)
        }
    };
    let mut rules = Vec::with_capacity(ruleset.definition.rules.len());
    for rule in &ruleset.definition.rules {
        rules.push(json!({"__typename":"RepositoryRule","type":upper(rule.name())?}));
    }
    let root = match ruleset.owner {
        Owner::Organization(id) | Owner::Repository(id) => id,
    };
    let mut value = json!({"__typename":"RepositoryRuleset","id":node_id::scoped("RRS",root,ruleset.id),"databaseId":ruleset.id,
        "name":ruleset.definition.name,"target":upper(ruleset.definition.target)?,"enforcement":upper(ruleset.definition.enforcement)?,
        "source":source,"createdAt":ruleset.created_at,"updatedAt":ruleset.updated_at,"_rules":rules});
    match ruleset.owner {
        Owner::Repository(repo) => {
            value["_repo"] = json!(repo);
            value["_scope"] = json!("metadata");
        }
        Owner::Organization(org) => {
            value["_ownerKind"] = json!("org");
            value["_owner"] = json!(org);
        }
    }
    Ok(value)
}

/// The rules of one listed ruleset, optionally narrowed to one rule type.
pub(super) fn rules(ctx: &Arguments, parent: &Value) -> Result<Value> {
    let kind = match ctx.args.get("type") {
        Some(value) if !value.is_null() => Some(value.enum_name()?),
        _ => None,
    };
    let rules = parent["_rules"]
        .as_array()
        .ok_or_else(missing)?
        .iter()
        .filter(|rule| kind.is_none_or(|kind| rule["type"] == kind))
        .cloned()
        .collect();
    crate::connection::slice(ctx, rules)
}
