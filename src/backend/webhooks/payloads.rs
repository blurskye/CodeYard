//! Payloads in the shape GitHub documents for each event, as far as this data model has the
//! fields. Fields without a Codeyard counterpart are left out rather than invented.
use crate::backend::api::github::{node_id, repo_url};
use crate::backend::api::http::config::{http_clone_base, link_origin};
use crate::backend::repositories::store::CommitInfo;
use codeyard_db_core::models::CommitComment;
use codeyard_db_core::models::DeployKey;
use codeyard_db_core::models::Issue;
use codeyard_db_core::models::IssueComment;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::PullRequestComment;
use codeyard_db_core::models::PullRequestReview;
use codeyard_db_core::models::Release;
use codeyard_db_core::models::ReleaseAsset;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use serde_json::{json, Value};
use std::path::Path;
mod code_scanning;
pub use code_scanning::{code_scanning_alert, code_scanning_instance};
mod gists;
pub use gists::{gist, gist_comment, gist_file, gist_revision};

/// The repository an event happened in, and the user who caused it.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub repo: &'a Repository,
    pub owner: &'a str,
    pub sender: &'a Person,
}

/// A user as a payload names them.
#[derive(Debug, Clone)]
pub struct Person {
    pub login: String,
    pub id: i64,
    pub site_admin: bool,
}

impl Person {
    pub fn of(user: &User) -> Self {
        Self {
            login: user.username.clone(),
            id: user.id,
            site_admin: user.is_admin,
        }
    }
}

fn time(value: chrono::DateTime<chrono::Utc>) -> Value {
    json!(value.to_rfc3339())
}

fn optional_time(value: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    value.map_or(Value::Null, time)
}

pub fn team(access: &crate::backend::db::repositories::push_teams::TeamAccess) -> Value {
    team_document(&access.team, &access.org_slug, access.permission)
}

pub fn team_document(team: &codeyard_db_core::models::Team, org: &str, permission: i16) -> Value {
    let org = codeyard_webui::encode_component(org);
    let slug = codeyard_webui::encode_component(&team.slug);
    let url = crate::backend::api::github::api_url(&format!("/teams/{}", team.id));
    let permission = match permission {
        1 => "pull",
        2 => "triage",
        3 => "push",
        4 => "maintain",
        5 => "admin",
        _ => "none",
    };
    json!({"id": team.id, "node_id": node_id::encode("T", team.id), "name": team.name,
        "slug": team.slug, "description": team.description, "privacy": if team.privacy == "secret" { "secret" } else { "closed" },
        "permission": permission, "type": "organization", "organization_id": team.org_id, "url": url,
        "notification_setting": if team.notifications_enabled { "notifications_enabled" } else { "notifications_disabled" },
        "html_url": format!("{}/orgs/{org}/teams/{slug}", link_origin()), "members_url": format!("{url}/members{{/member}}"),
        "repositories_url": format!("{url}/repos"), "parent": null})
}

pub fn repository_property(
    definition: &crate::backend::db::organizations::properties::Definition,
    org: &str,
) -> Value {
    let mut value = json!(definition.fields);
    value["source_type"] = json!("organization");
    value["url"] = json!(crate::backend::api::github::api_url(&format!(
        "/orgs/{}/properties/schema/{}",
        codeyard_webui::encode_component(org),
        codeyard_webui::encode_component(&definition.fields.property_name)
    )));
    value
}
pub fn repository_property_values(
    values: &crate::backend::db::organizations::properties::RepositoryValues,
    org: &str,
) -> Value {
    json!({"repository_id":values.repository.id,"repository_name":values.repository.name,"repository_full_name":format!("{org}/{}",values.repository.name),"properties":values.properties})
}

pub fn organization_role(
    role: &crate::backend::db::organizations::roles::OrganizationRole,
) -> Value {
    let base = match role.base_level {
        1 => Some("read"),
        2 => Some("triage"),
        3 => Some("write"),
        4 => Some("maintain"),
        5 => Some("admin"),
        _ => None,
    };
    let permissions = if role.id
        == crate::backend::db::organizations::roles::SECURITY_MANAGER_ROLE_ID
        && role.base_level == 1
    {
        vec!["manage_security"]
    } else {
        Vec::new()
    };
    json!({"id":role.id,"name":role.name,"description":role.description,"base_role":base,"source":"Predefined","permissions":permissions,"organization":null,"created_at":time(role.created_at),"updated_at":time(role.updated_at)})
}
pub fn organization_role_user(
    assignment: &crate::backend::db::organizations::roles::UserAssignment,
    org: &str,
    level: i16,
) -> Value {
    let mut value = user(&assignment.user);
    value["assignment"] = json!(
        match (assignment.direct, assignment.inherited_from.is_empty()) {
            (true, true) => "direct",
            (false, _) => "indirect",
            (true, false) => "mixed",
        }
    );
    value["inherited_from"] = json!(assignment
        .inherited_from
        .iter()
        .map(
            |t| team(&crate::backend::db::repositories::push_teams::TeamAccess {
                team: t.clone(),
                org_slug: org.into(),
                permission: level
            })
        )
        .collect::<Vec<_>>());
    value
}

/// An organization's picture: its uploaded file, or the generated one.
pub fn organization_avatar(org: &codeyard_db_core::models::Organization) -> String {
    org.avatar_file_id.map_or_else(
        || {
            format!(
                "{}/avatars/{}",
                link_origin(),
                codeyard_webui::encode_component(&org.name)
            )
        },
        |id| format!("{}/api/files/{id}/download", link_origin()),
    )
}

/// Organization links are shared by membership responses and organization events.
pub fn organization(org: &codeyard_db_core::models::Organization) -> Value {
    let name = codeyard_webui::encode_component(&org.name);
    let url = crate::backend::api::github::api_url(&format!("/orgs/{name}"));
    let avatar = organization_avatar(org);
    json!({"login": org.name, "id": org.id, "node_id": node_id::encode("O", org.id),
        "url": url, "repos_url": format!("{url}/repos"), "events_url": format!("{url}/events"),
        "hooks_url": format!("{url}/hooks"), "issues_url": format!("{url}/issues"),
        "members_url": format!("{url}/members{{/member}}"),
        "public_members_url": format!("{url}/public_members{{/member}}"),
        "avatar_url": avatar, "description": org.description})
}

pub fn organization_invitation(
    invitation: &codeyard_db_core::models::OrgInvitation,
    org: &codeyard_db_core::models::Organization,
    recipient: Option<&User>,
    inviter: Option<&User>,
    team_count: i64,
) -> Value {
    let inviter = inviter.map(Person::of).unwrap_or(Person {
        login: "ghost".into(),
        id: 0,
        site_admin: false,
    });
    json!({"id": invitation.id, "node_id": node_id::encode("OI", invitation.id),
        "login": recipient.map(|user| &user.username), "email": invitation.email,
        "role": if invitation.role == "owner" { "admin" } else { "direct_member" },
        "created_at": time(invitation.created_at), "failed_at": optional_time(invitation.failed_at), "failed_reason": invitation.failed_reason,
        "inviter": person(&inviter), "team_count": team_count, "invitation_source": "member",
        "invitation_teams_url": crate::backend::api::github::api_url(&format!("/orgs/{}/invitations/{}/teams", codeyard_webui::encode_component(&org.name), invitation.id))})
}

pub fn repository_traffic(
    kind: crate::backend::repositories::traffic::Kind,
    summary: &crate::backend::repositories::traffic::Summary,
) -> Value {
    use crate::backend::repositories::traffic::Kind;
    let key = match kind {
        Kind::Views => "views",
        Kind::Clones => "clones",
    };
    let buckets:Vec<_>=summary.buckets.iter().map(|bucket|json!({"timestamp":bucket.timestamp,"count":bucket.count,"uniques":bucket.uniques})).collect();
    json!({"count":summary.totals.count,"uniques":summary.totals.uniques,(key):buckets})
}
pub fn content_traffic(path: &crate::backend::repositories::traffic::PopularPath) -> Value {
    json!({"path":path.path,"title":path.title,"count":path.count,"uniques":path.uniques})
}
pub fn referrer_traffic(
    referrer: &crate::backend::repositories::traffic::PopularReferrer,
) -> Value {
    json!({"referrer":referrer.referrer,"count":referrer.count,"uniques":referrer.uniques})
}

pub fn repository_activity(
    activity: &crate::backend::db::repositories::activity::Activity,
) -> Value {
    let actor = activity
        .actor_id
        .zip(activity.actor.as_ref())
        .map(|(id, login)| {
            simple_person(&Person {
                id,
                login: login.clone(),
                site_admin: false,
            })
        });
    json!({"id":activity.id,"node_id":crate::backend::api::github::node_id::encode("RAC",activity.id),"before":activity.before_sha,"after":activity.after_sha,"ref":activity.reference,"timestamp":time(activity.created_at),"activity_type":activity.activity_type,"actor":actor})
}

pub fn release_notes(notes: &crate::backend::repositories::releases::ReleaseNotes) -> Value {
    json!({"name":notes.name,"body":notes.body})
}

pub fn autolink(link: &crate::backend::db::repositories::autolinks::Autolink) -> Value {
    json!({"id":link.id,"key_prefix":link.key_prefix,"url_template":link.url_template,"is_alphanumeric":link.is_alphanumeric})
}

pub fn community_file(file: Option<&crate::backend::repositories::community::File>) -> Value {
    let Some(file) = file else {
        return Value::Null;
    };
    let encode = codeyard_webui::encode_component;
    let path = codeyard_webui::pages::repos::ref_path::encode_path(&file.path);
    let base = format!(
        "/repos/{}/{}",
        encode(&file.owner),
        encode(&file.repository)
    );
    json!({"url":crate::backend::api::github::api_url(&format!("{base}/contents/{path}?ref={}",file.reference)),"html_url":format!("{}{}",link_origin(),codeyard_webui::pages::repos::ref_path::file_url(&format!("/{}/{}",encode(&file.owner),encode(&file.repository)),"blob",&file.reference,&file.path))})
}

pub fn community_profile(profile: &crate::backend::repositories::community::Profile) -> Value {
    use crate::backend::api::github::{api_url, node_id};
    let conduct = community_file(profile.conduct.as_ref());
    let license = match (profile.license_info, profile.license.as_ref()) {
        (Some(info), Some(file)) => {
            json!({"key":info.key,"name":info.name,"spdx_id":info.spdx,"node_id":node_id::git("LIC",info.key),"url":if info.key=="other" {Value::Null}else {json!(api_url(&format!("/licenses/{}",info.key)))},"html_url":community_file(Some(file))["html_url"]})
        }
        _ => Value::Null,
    };
    json!({"health_percentage":profile.health_percentage(),"description":profile.description,"documentation":profile.documentation,"updated_at":profile.updated_at,
        "files":{"code_of_conduct":if profile.contributor_covenant {json!({"key":"contributor_covenant","name":"Contributor Covenant","html_url":conduct["html_url"],"url":api_url("/codes_of_conduct/contributor_covenant")})}else {Value::Null},"code_of_conduct_file":conduct,"license":license,"readme":community_file(profile.readme.as_ref()),"contributing":community_file(profile.contributing.as_ref()),"issue_template":community_file(profile.issue_template.as_ref()),"pull_request_template":community_file(profile.pull_request_template.as_ref())}})
}

pub fn collaborator(user: &User, level: codeyard_db_core::models::AccessLevel) -> Value {
    let mut value = simple_person(&Person::of(user));
    value["site_admin"] = json!(user.is_admin);
    value["role_name"] = json!(if level.can_admin() {
        "admin"
    } else {
        level.as_str()
    });
    value["permissions"] = json!({"pull":level.can_read(), "triage":level>=codeyard_db_core::models::AccessLevel::Triage, "push":level.can_write(), "maintain":level.can_maintain(), "admin":level.can_admin()});
    value
}

pub fn repository_invitation(
    invitation: &crate::backend::db::repositories::collaborators::Invitation,
    repo: Value,
    inviter: &Person,
    invitee: &Person,
) -> Value {
    json!({"id":invitation.id, "node_id":crate::backend::api::github::node_id::encode("RI",invitation.id),
        "url":crate::backend::api::github::api_url(&format!("/user/repository_invitations/{}",invitation.id)),
        "html_url":format!("{}/invitations", repo["html_url"].as_str().unwrap_or("")),
        "repository":repo, "inviter":simple_person(inviter), "invitee":simple_person(invitee),
        "permissions":codeyard_db_core::models::AccessLevel::from(invitation.permission).as_str(), "created_at":invitation.created_at, "expired":false})
}

pub fn deploy_key(key: &DeployKey, owner: &str, name: &str, added_by: Option<&str>) -> Value {
    json!({"id": key.id, "key": key.public_key, "title": key.title,
        "url": format!("{}/keys/{}", crate::backend::api::github::repo_url(owner, name), key.id),
        "verified": true, "created_at": key.created_at, "read_only": !key.can_write,
        "added_by": added_by, "last_used": key.last_used_at})
}

pub fn webhook_config(hook: &codeyard_db_core::models::Webhook) -> Value {
    let mut config = json!({"url": hook.payload_url,
        "content_type": if hook.content_type == "application/json" { "json" } else { "form" },
        "insecure_ssl": if hook.ssl_verify { "0" } else { "1" },
        "secret": if hook.encrypted_secret.is_some() { "********" } else { "" }});
    if let Some(username) = &hook.username {
        config["username"] = json!(username);
        config["password"] = json!(if hook.encrypted_password.is_some() {
            "********"
        } else {
            ""
        });
    }
    config
}

pub fn webhook(hook: &codeyard_db_core::models::Webhook, url: &str) -> Value {
    json!({"id": hook.id, "url": url, "name": "web",
        "type": if hook.org_id.is_some() { "Organization" } else { "Repository" },
        "active": hook.active, "events": hook.events.split(',').collect::<Vec<_>>(),
        "config": webhook_config(hook), "created_at": time(hook.created_at),
        "updated_at": time(hook.updated_at), "ping_url": format!("{url}/pings"),
        "deliveries_url": format!("{url}/deliveries")})
}

fn delivery_payload(delivery: &codeyard_db_core::models::WebhookDelivery) -> Value {
    serde_json::from_str(&delivery.request_body).unwrap_or_else(|_| {
        url::form_urlencoded::parse(delivery.request_body.as_bytes())
            .find(|(key, _)| key == "payload")
            .and_then(|(_, payload)| serde_json::from_str(&payload).ok())
            .unwrap_or(Value::Null)
    })
}

pub fn webhook_delivery(
    delivery: &codeyard_db_core::models::WebhookDelivery,
    detail: bool,
) -> Value {
    let payload = delivery_payload(delivery);
    let status = delivery.error.clone().unwrap_or_else(|| {
        delivery
            .status_code
            .and_then(|code| u16::try_from(code).ok())
            .and_then(|code| axum::http::StatusCode::from_u16(code).ok())
            .and_then(|code| code.canonical_reason().map(str::to_owned))
            .unwrap_or_else(|| "No response".into())
    });
    let mut value = json!({"id": delivery.id, "guid": delivery.guid, "delivered_at": time(delivery.delivered_at),
        "redelivery": delivery.redelivery, "duration": f64::from(delivery.duration_ms) / 1000.0,
        "status": status, "status_code": delivery.status_code.unwrap_or(0),
        "event": delivery.event, "action": delivery.action, "installation_id": null,
        "repository_id": payload.pointer("/repository/id").and_then(Value::as_i64), "throttled_at": null});
    if detail {
        let headers = |text: &str| -> Value {
            Value::Object(
                text.lines()
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.trim().to_owned(), json!(value.trim())))
                    .collect(),
            )
        };
        value["request"] =
            json!({"headers": headers(&delivery.request_headers), "payload": payload});
        value["response"] = json!({"headers": delivery.response_headers.as_deref().map(headers), "payload": delivery.response_body});
    }
    value
}

pub fn deployment(
    deployment: &crate::backend::db::deployments::Deployment,
    creator: Option<Value>,
    payload: Value,
    owner: &str,
    repo: &str,
) -> Value {
    let base = repo_url(owner, repo);
    let url = format!("{base}/deployments/{}", deployment.id);
    json!({"id": deployment.id, "node_id": node_id::encode("DE", deployment.id), "url": url,
        "sha": deployment.sha, "ref": deployment.reference, "task": deployment.task, "payload": payload,
        "original_environment": deployment.original_environment, "environment": deployment.environment,
        "description": deployment.description, "creator": creator, "created_at": time(deployment.created_at),
        "updated_at": time(deployment.updated_at), "statuses_url": format!("{url}/statuses"), "repository_url": base,
        "transient_environment": deployment.transient_environment, "production_environment": deployment.production_environment,
        "performed_via_github_app": null})
}

pub fn deployment_status(
    status: &crate::backend::db::deployments::Status,
    creator: Option<Value>,
    owner: &str,
    repo: &str,
) -> Value {
    let base = repo_url(owner, repo);
    let deployment = format!("{base}/deployments/{}", status.deployment_id);
    // Without an external log, the saved deployment is the available status information.
    let target = if status.log_url.is_empty() {
        &deployment
    } else {
        &status.log_url
    };
    let mut value = json!({"id": status.id, "node_id": node_id::encode("DES", status.id), "state": status.state,
        "url": format!("{deployment}/statuses/{}", status.id), "creator": creator, "description": status.description,
        "environment": status.environment, "target_url": target,
        "created_at": time(status.created_at), "updated_at": time(status.created_at),
        "deployment_url": deployment, "repository_url": base, "performed_via_github_app": null});
    if !status.log_url.is_empty() {
        value["log_url"] = json!(status.log_url);
    }
    if !status.environment_url.is_empty() {
        value["environment_url"] = json!(status.environment_url);
    }
    value
}

pub fn team_organization(
    org: &codeyard_db_core::models::Organization,
    public_repos: i64,
    has_organization_projects: bool,
) -> Value {
    let slug = codeyard_webui::encode_component(&org.name);
    let url = crate::backend::api::github::api_url(&format!("/orgs/{slug}"));
    json!({"id":org.id,"node_id":node_id::encode("O",org.id),"login":org.name,
        "name":org.display_name.as_deref().unwrap_or(&org.name),"description":org.description,"url":url,
        "html_url":format!("{}{}",link_origin(),codeyard_webui::profile_path(&org.name)),"avatar_url":organization_avatar(org),
        "repos_url":format!("{url}/repos"),"events_url":format!("{url}/events"),
        "hooks_url":format!("{url}/hooks"),"issues_url":format!("{url}/issues"),
        "members_url":format!("{url}/members{{/member}}"),"public_members_url":format!("{url}/public_members{{/member}}"),
        "has_organization_projects":has_organization_projects,"has_repository_projects":false,"public_repos":public_repos,
        "public_gists":0,"followers":0,"following":0,"type":"Organization",
        "created_at":time(org.created_at),"updated_at":time(org.updated_at),"archived_at":null})
}

pub fn environment(
    environment: &crate::backend::db::environments::Environment,
    owner: &str,
    repo: &str,
    reviewers: Vec<Value>,
) -> Value {
    let name = codeyard_webui::encode_component(&environment.name);
    let mut rules = Vec::new();
    let rule = |kind: &str| json!({"id": environment.id, "node_id": node_id::encode("G", environment.id), "type": kind});
    if environment.wait_timer > 0 {
        let mut value = rule("wait_timer");
        value["wait_timer"] = json!(environment.wait_timer);
        rules.push(value);
    }
    if serde_json::from_str::<Vec<crate::backend::db::environments::Reviewer>>(
        &environment.reviewers,
    )
    .is_ok_and(|stored| !stored.is_empty())
    {
        let mut value = rule("required_reviewers");
        value["prevent_self_review"] = json!(environment.prevent_self_review);
        value["reviewers"] = json!(reviewers);
        rules.push(value);
    }
    if environment.branch_policy.is_some() {
        rules.push(rule("branch_policy"));
    }
    json!({"id": environment.id, "node_id": node_id::encode("EN", environment.id), "name": environment.name,
        "url": format!("{}/environments/{name}", repo_url(owner, repo)),
        "html_url": format!("{}/settings?tab=secrets&environment={name}", repository_url(owner, repo)),
        "created_at": time(environment.created_at), "updated_at": time(environment.updated_at),
        "protection_rules": rules, "deployment_branch_policy": environment.branch_policy.as_ref().map(|policy| json!({"protected_branches": policy == "protected", "custom_branch_policies": policy == "custom"}))})
}

pub fn person(person: &Person) -> Value {
    simple_person(person)
}

/// A public account document shared by REST objects and webhook senders.
pub fn user(user: &User) -> Value {
    let mut value = person(&Person::of(user));
    value["site_admin"] = json!(user.is_admin);
    value
}

/// Certified OpenPGP material; email confirmation is live account state, never a key claim.
pub fn gpg_key(
    key: &crate::backend::db::users::gpg_keys::GpgKey,
    subkeys: &[crate::backend::db::users::gpg_keys::GpgKey],
    verified_emails: &[String],
) -> Value {
    let material = &key.material;
    json!({
        "id":key.id,"name":key.name,"primary_key_id":key.primary_key_id,"key_id":key.key_id,
        "public_key":material.public_key,"raw_key":material.raw_key,
        "emails":material.emails.iter().map(|email|json!({"email":email,"verified":verified_emails.iter().any(|verified|verified.eq_ignore_ascii_case(email))})).collect::<Vec<_>>(),
        "subkeys":subkeys.iter().filter(|subkey|subkey.primary_key_id==Some(key.id)).map(|subkey|gpg_key(subkey,&[],verified_emails)).collect::<Vec<_>>(),
        "can_sign":material.can_sign,"can_certify":material.can_certify,
        "can_encrypt_comms":material.can_encrypt_comms,"can_encrypt_storage":material.can_encrypt_storage,
        "created_at":material.created_at,"expires_at":material.expires_at,"revoked":material.revoked,
    })
}

/// Public account fields; contact email is not published without an explicit visibility choice.
pub fn public_user(
    user: &User,
    profile: &crate::backend::db::users::profiles::ProfileSettings,
    public_repos: i64,
    followers: i64,
    following: i64,
    public_gists: i64,
    statistics: Option<&crate::backend::db::users::profiles::AccountStatistics>,
) -> Value {
    let mut value = self::user(user);
    value["name"] = json!(user.display_name);
    value["bio"] = json!(user.bio);
    value["location"] = json!(user.location);
    value["blog"] = json!(user.website.as_deref().unwrap_or(""));
    value["company"] = json!(profile.company);
    value["email"] = json!(profile.public_email());
    value["hireable"] = json!(profile.hireable);
    value["twitter_username"] = json!(profile.twitter_username);
    value["public_repos"] = json!(public_repos);
    value["public_gists"] = json!(public_gists);
    value["followers"] = json!(followers);
    value["following"] = json!(following);
    value["created_at"] = time(user.created_at);
    value["updated_at"] = time(user.updated_at);
    if let Some(statistics) = statistics {
        value["owned_private_repos"] = json!(statistics.owned_private_repos);
        value["total_private_repos"] = json!(statistics.total_private_repos);
        value["disk_usage"] = json!(statistics.disk_usage);
        value["collaborators"] = json!(statistics.collaborators);
        value["private_gists"] = json!(statistics.private_gists);
        value["two_factor_authentication"] = json!(user.two_factor_enabled);
    }
    value
}

pub fn package_url(
    owner: &crate::backend::db::packages::Owner,
    package: &crate::backend::db::packages::Package,
) -> String {
    let namespace = if owner.kind == "org" { "orgs" } else { "users" };
    format!(
        "/{namespace}/{}/packages/{}/{}",
        codeyard_webui::encode_component(&owner.name),
        package.package_type,
        codeyard_webui::encode_component(&package.name)
    )
}

pub fn package(
    owner: &crate::backend::db::packages::Owner,
    package: &crate::backend::db::packages::Package,
    version_count: i64,
    repository: Option<Value>,
) -> Value {
    let path = package_url(owner, package);
    let account = if owner.kind == "org" {
        organization_person(owner.id, &owner.name)
    } else {
        simple_person(&Person {
            id: owner.id,
            login: owner.name.clone(),
            site_admin: false,
        })
    };
    json!({"id":package.id,"name":package.name,"package_type":package.package_type,
        "visibility":package.visibility,"owner":account,"repository":repository,
        "url":crate::backend::api::github::api_url(&path),"html_url":format!("{}{path}",link_origin()),
        "version_count":version_count,"created_at":package_time(package.created_at),"updated_at":package_time(package.updated_at)})
}

fn package_time(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

pub fn package_version(
    owner: &crate::backend::db::packages::Owner,
    package: &crate::backend::db::packages::Package,
    version: &crate::backend::db::packages::Version,
    tags: &[String],
) -> Value {
    let path = package_url(owner, package);
    let mut metadata = json!({"package_type":package.package_type});
    if package.package_type == "container" {
        metadata["container"] = json!({"tags":tags});
    }
    let mut value = json!({"id":version.id,"name":version.name,
        "url":crate::backend::api::github::api_url(&format!("{path}/versions/{}",version.id)),
        "package_html_url":format!("{}{path}",link_origin()),"html_url":format!("{}{path}/versions/{}",link_origin(),version.id),
        "created_at":package_time(version.created_at),"updated_at":package_time(version.updated_at),"metadata":metadata});
    if let Some(deleted) = version.deleted_at {
        value["deleted_at"] = json!(package_time(deleted));
    }
    value
}

fn repository_url(owner: &str, name: &str) -> String {
    format!(
        "{}{}",
        link_origin(),
        codeyard_webui::repository_path(owner, name)
    )
}

/// The public links shared by REST objects and webhook actors.
pub fn simple_person(person: &Person) -> Value {
    let origin = link_origin();
    let login = codeyard_webui::encode_component(&person.login);
    let url = format!("{origin}/api/v3/users/{login}");
    let mut value = json!({
        "login": person.login, "id": person.id,
        "node_id": crate::backend::api::github::node_id::encode("U", person.id),
        "type": if person.login.ends_with("[bot]") { "Bot" } else { "User" },
        "html_url": format!("{origin}{}",codeyard_webui::profile_path(&person.login)), "url": url,
        "avatar_url": format!("{origin}/avatars/{login}"),
        "gravatar_id": "", "site_admin": person.site_admin,
    });
    for (field, suffix) in [
        ("followers_url", "/followers"),
        ("following_url", "/following{/other_user}"),
        ("gists_url", "/gists{/gist_id}"),
        ("starred_url", "/starred{/owner}{/repo}"),
        ("subscriptions_url", "/subscriptions"),
        ("organizations_url", "/orgs"),
        ("repos_url", "/repos"),
        ("events_url", "/events{/privacy}"),
        ("received_events_url", "/received_events"),
    ] {
        value[field] = json!(format!("{url}{suffix}"));
    }
    value
}

/// Project documents are shared by REST and project events.
pub fn project(
    project: &crate::backend::db::projects::Project,
    owner: Value,
    creator: Value,
) -> Value {
    json!({
        "id": project.id, "node_id": node_id::encode("PVT", project.id),
        "owner": owner, "creator": creator, "number": project.number,
        "title": project.title, "description": project.description,
        "short_description": project.description, "public": project.public,
        "closed_at": optional_time(project.closed_at), "deleted_at": optional_time(project.deleted_at),
        "deleted_by": null, "created_at": time(project.created_at), "updated_at": time(project.updated_at),
        "state": if project.closed_at.is_some() { "closed" } else { "open" },
        "is_template": project.template, "latest_status_update": null,
    })
}

pub fn project_field(
    field: &crate::backend::db::projects::Field,
    project_url: &str,
) -> anyhow::Result<Value> {
    let config = crate::backend::db::projects::field_configuration(field)?;
    let prefix = match field.data_type.as_str() {
        "single_select" => "PVTSSF",
        "iteration" => "PVTIF",
        _ => "PVTF",
    };
    let mut value = json!({"id": field.id, "node_id": node_id::encode(prefix, field.id),
        "name": field.name, "data_type": field.data_type, "project_url": project_url,
        "created_at": time(field.created_at), "updated_at": time(field.updated_at)});
    let rich_text =
        |text: &str| json!({"raw": text, "html": codeyard_webui::components::escape_html(text)});
    if let Some(options) = config["options"].as_array() {
        value["options"] = json!(options.iter().map(|option| json!({"id": option["id"],
            "name": rich_text(option["name"].as_str().unwrap_or("")),
            "description": rich_text(option["description"].as_str().unwrap_or("")), "color": option["color"]})).collect::<Vec<_>>());
    }
    if field.data_type == "iteration" {
        use chrono::Datelike;
        let date = chrono::NaiveDate::parse_from_str(
            config["start_date"].as_str().unwrap_or(""),
            "%Y-%m-%d",
        )?;
        let iterations = config["iterations"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid iteration configuration"))?;
        value["configuration"] = json!({"start_day": date.weekday().number_from_monday(), "duration": config["duration"],
            "iterations": iterations.iter().map(|iteration| json!({"id": iteration["id"], "title": rich_text(iteration["title"].as_str().unwrap_or("")), "start_date": iteration["start_date"], "duration": iteration["duration"], "completed": iteration["start_date"].as_str().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()).zip(iteration["duration"].as_i64()).is_some_and(|(start, days)| start.checked_add_signed(chrono::Duration::days(days)).is_some_and(|end| end <= chrono::Utc::now().date_naive()))})).collect::<Vec<_>>()});
    }
    Ok(value)
}

pub fn project_item(
    item: &crate::backend::db::projects::Item,
    project_url: &str,
    creator: Value,
    content: Value,
) -> Value {
    json!({"id": item.id, "node_id": node_id::encode("PVTI",item.id),
        "project_url": project_url, "item_url": format!("{project_url}/items/{}", item.id),
        "content_type": item.content_type(), "content": content, "creator": creator,
        "created_at": time(item.created_at), "updated_at": time(item.updated_at),
        "is_archived": item.archived, "archived_at": optional_time(item.archived_at)})
}

pub fn project_draft(item: &crate::backend::db::projects::Item, creator: Value) -> Value {
    json!({"id": item.id, "node_id": node_id::encode("DI",item.id), "title": item.draft_title,
        "body": item.draft_body, "user": creator, "created_at": time(item.created_at), "updated_at": time(item.updated_at)})
}

pub fn project_view(
    view: &crate::backend::db::projects::View,
    project_url: &str,
    html_url: &str,
    creator: Value,
) -> anyhow::Result<Value> {
    let config: crate::backend::db::projects::NewView = serde_json::from_str(&view.configuration)?;
    let mut value = serde_json::to_value(config)?;
    let map = value.as_object_mut().expect("view configuration object");
    map.extend(
        json!({"id": view.id, "number": view.number, "node_id": node_id::encode("PVTV",view.id),
        "project_url": project_url, "html_url": html_url, "creator": creator,
        "created_at": time(view.created_at), "updated_at": time(view.updated_at)})
        .as_object()
        .expect("view object")
        .clone(),
    );
    Ok(value)
}

pub fn organization_person(id: i64, name: &str) -> Value {
    let mut value = simple_person(&Person {
        id,
        login: name.into(),
        site_admin: false,
    });
    value["type"] = json!("Organization");
    value["node_id"] = json!(node_id::encode("O", id));
    value["repos_url"] = json!(crate::backend::api::github::api_url(&format!(
        "/orgs/{}/repos",
        codeyard_webui::encode_component(name)
    )));
    value
}
pub fn topic(topic: &crate::backend::db::catalog_search::Topic) -> Value {
    json!({"name":topic.name,"display_name":topic.display_name,"short_description":topic.short_description,"description":topic.description,"created_by":topic.created_by,"released":topic.released,"featured":topic.featured,"curated":topic.curated,"created_at":time(topic.created_at),"updated_at":time(topic.updated_at),"repository_count":topic.repository_count})
}
pub fn repository_owner(repo: &Repository, owner: &str) -> Value {
    if repo.owner_kind == "org" {
        return organization_person(repo.owner_id(), owner);
    }
    simple_person(&Person {
        login: owner.to_owned(),
        id: repo.owner_id(),
        site_admin: false,
    })
}

pub fn repository_snapshot(
    snapshot: &crate::backend::db::repository_stats::RepositorySnapshot,
) -> Value {
    let mut value = repository(&snapshot.repository, &snapshot.owner);
    value["open_issues"] = json!(snapshot.facts.open_items);
    value["open_issues_count"] = json!(snapshot.facts.open_items);
    value["pushed_at"] = json!(snapshot.facts.pushed_at);
    value["language"] = json!(snapshot.facts.language);
    value
}

/// Replaces the zero fork counts of an unscoped repository document with the count its
/// audience may see.
pub fn set_fork_counts(repository: &mut Value, count: i32) {
    if !repository.is_object() {
        return;
    }
    for field in ["forks", "forks_count", "network_count"] {
        repository[field] = json!(count);
    }
}

/// GitHub's repository switches, as REST documents and webhook payloads report them.
pub fn set_features(
    repository: &mut serde_json::Map<String, Value>,
    features: &crate::backend::db::repositories::records::RepositoryFeatures,
) {
    for (key, value) in [
        ("has_wiki", json!(features.wiki)),
        ("has_issues", json!(features.has_issues)),
        ("has_projects", json!(features.has_projects)),
        ("allow_forking", json!(features.allow_forking)),
        (
            "delete_branch_on_merge",
            json!(features.delete_branch_on_merge),
        ),
        ("allow_update_branch", json!(features.allow_update_branch)),
        ("allow_auto_merge", json!(features.allow_auto_merge)),
        (
            "squash_merge_commit_title",
            json!(features.squash_merge_commit_title),
        ),
        (
            "squash_merge_commit_message",
            json!(features.squash_merge_commit_message),
        ),
    ] {
        repository.insert(key.into(), value);
    }
}

/// GitHub's lowercase visibility name for a repository.
pub fn visibility(repo: &Repository) -> &'static str {
    match (repo.is_internal, repo.is_private) {
        (true, _) => "internal",
        (false, true) => "private",
        (false, false) => "public",
    }
}

pub fn repository(repo: &Repository, owner: &str) -> Value {
    let full_name = format!("{owner}/{}", repo.name);
    let html_url = repository_url(owner, &repo.name);
    let api = crate::backend::api::github::repo_url(owner, &repo.name);
    let ssh = crate::backend::api::http::config::ssh_clone_base();
    let ssh_url = (!ssh.trim().is_empty())
        .then(|| format!("{}/{owner}/{}.git", ssh.trim_end_matches('/'), repo.name));
    let mut value = json!({
        "id": repo.id,
        "node_id": crate::backend::api::github::node_id::encode("R", repo.id),
        "url": api,
        "name": repo.name,
        "full_name": full_name,
        "private": repo.is_private,
        "owner": repository_owner(repo, owner),
        "html_url": html_url,
        "clone_url": format!("{}/{owner}/{}.git", http_clone_base().trim_end_matches('/'), repo.name),
        "git_url": format!("{}/{owner}/{}.git", http_clone_base().trim_end_matches('/'), repo.name),
        "ssh_url": ssh_url,
        "svn_url": html_url, "homepage": repo.website, "language": null, "license": null,
        "mirror_url": null, "pushed_at": null, "size": repo.size_bytes / 1024,
        "has_issues": true, "has_downloads": true, "has_projects": false, "has_wiki": false,
        "has_pages": false, "has_discussions": false, "disabled": false,
        "watchers": repo.stars_count, "open_issues": repo.open_issues_count,
        "description": repo.description,
        "fork": repo.is_fork,
        // The cached total includes private forks, so the builder reports none; readers that
        // know their audience replace these with set_fork_counts.
        "forks": 0, "forks_count": 0, "network_count": 0,
        "is_template": repo.is_template,
        "archived": repo.is_archived,
        "default_branch": repo.default_branch,
        "stargazers_count": repo.stars_count,
        "watchers_count": repo.stars_count,
        "subscribers_count": repo.watchers_count,
        "open_issues_count": repo.open_issues_count,
        "created_at": time(repo.created_at),
        "updated_at": time(repo.updated_at),
        "visibility": visibility(repo),
        "allow_merge_commit": repo.allow_merge_commit,
        "allow_squash_merge": repo.allow_squash_merge,
        "allow_rebase_merge": repo.allow_rebase_merge,
    });
    for (field, suffix) in [
        ("archive_url", "/{archive_format}{/ref}"),
        ("assignees_url", "/assignees{/user}"),
        ("blobs_url", "/git/blobs{/sha}"),
        ("branches_url", "/branches{/branch}"),
        ("collaborators_url", "/collaborators{/collaborator}"),
        ("comments_url", "/comments{/number}"),
        ("commits_url", "/commits{/sha}"),
        ("compare_url", "/compare/{base}...{head}"),
        ("contents_url", "/contents/{+path}"),
        ("contributors_url", "/contributors"),
        ("deployments_url", "/deployments"),
        ("downloads_url", "/downloads"),
        ("events_url", "/events"),
        ("forks_url", "/forks"),
        ("git_commits_url", "/git/commits{/sha}"),
        ("git_refs_url", "/git/refs{/sha}"),
        ("git_tags_url", "/git/tags{/sha}"),
        ("hooks_url", "/hooks"),
        ("issue_comment_url", "/issues/comments{/number}"),
        ("issue_events_url", "/issues/events{/number}"),
        ("issues_url", "/issues{/number}"),
        ("keys_url", "/keys{/key_id}"),
        ("labels_url", "/labels{/name}"),
        ("languages_url", "/languages"),
        ("merges_url", "/merges"),
        ("milestones_url", "/milestones{/number}"),
        (
            "notifications_url",
            "/notifications{?since,all,participating}",
        ),
        ("pulls_url", "/pulls{/number}"),
        ("releases_url", "/releases{/id}"),
        ("stargazers_url", "/stargazers"),
        ("statuses_url", "/statuses/{sha}"),
        ("subscribers_url", "/subscribers"),
        ("subscription_url", "/subscription"),
        ("tags_url", "/tags"),
        ("teams_url", "/teams"),
        ("trees_url", "/git/trees{/sha}"),
    ] {
        value[field] = json!(format!("{api}{suffix}"));
    }
    value
}

/// The same saved comment is used by REST and hook deliveries.
pub fn commit_comment(
    comment: &CommitComment,
    author: Option<&Person>,
    association: &str,
    repo: &Repository,
    owner: &str,
    reactions: &[crate::backend::db::reactions::ReactionCount],
) -> Value {
    let url = repo_url(owner, &repo.name);
    json!({
        "id": comment.id, "node_id": node_id::encode("CC", comment.id), "body": comment.body,
        "user": author.map(person), "author_association": association,
        "url": format!("{url}/comments/{}", comment.id),
        "html_url": format!("{}{}", link_origin(), crate::backend::repositories::commit_comments::comment_link(owner, &repo.name, comment)),
        "commit_id": comment.sha, "path": comment.path, "position": comment.position, "line": comment.line,
        "reactions":reaction_rollup(&format!("{url}/comments/{}/reactions",comment.id),reactions),
        "created_at": time(comment.created_at), "updated_at": time(comment.updated_at),
    })
}

pub fn commit_comment_event(action: &str, comment: Value, scope: Scope<'_>) -> Value {
    json!({"action": action, "comment": comment, "repository": repository(scope.repo, scope.owner), "sender": person(scope.sender)})
}

/// The shape before the key check. The commit and tag API documents replace it with
/// `identity::signatures::verification`, which checks the signer's registered keys.
fn git_verification(signature: Option<&str>, payload: Option<&[u8]>) -> Value {
    let reason = match signature {
        None => "unsigned",
        Some(value)
            if (value.starts_with("-----BEGIN PGP SIGNATURE-----")
                && value.trim_end().ends_with("-----END PGP SIGNATURE-----"))
                || (value.starts_with("-----BEGIN SSH SIGNATURE-----")
                    && value.trim_end().ends_with("-----END SSH SIGNATURE-----")) =>
        {
            "gpgverify_unavailable"
        }
        Some(_) => "malformed_signature",
    };
    json!({"verified": false, "reason": reason, "signature": signature, "payload": payload.map(String::from_utf8_lossy), "verified_at": null})
}

/// GitHub's short commit object, as a workflow run names its head commit.
pub fn simple_commit(commit: &CommitInfo) -> Value {
    simple_commit_document(
        (&commit.sha, &commit.tree_sha, &commit.message),
        commit.committer_timestamp,
        (&commit.author_name, &commit.author_email),
        (&commit.committer_name, &commit.committer_email),
    )
}

/// The same short commit from the bounded snapshot already captured for a push event.
pub fn pushed_head(commit: &PushedCommit) -> Value {
    simple_commit_document(
        (&commit.id, &commit.tree_id, &commit.message),
        commit.committer_time,
        (&commit.author_name, &commit.author_email),
        (&commit.committer_name, &commit.committer_email),
    )
}

fn simple_commit_document(
    (id, tree_id, message): (&str, &str, &str),
    timestamp: i64,
    (author, author_email): (&str, &str),
    (committer, committer_email): (&str, &str),
) -> Value {
    json!({
        "id": id, "tree_id": tree_id, "message": message,
        "timestamp": chrono::DateTime::from_timestamp(timestamp, 0).map(time),
        "author": {"name": author, "email": author_email},
        "committer": {"name": committer, "email": committer_email},
    })
}

/// The Git object shared by the contents API and commit responses.
pub fn git_commit(commit: &CommitInfo, owner: &str, repo: &str, repo_id: i64) -> Value {
    let url = repo_url(owner, repo);
    let html = repository_url(owner, repo);
    let signature = |name: &str, email: &str, timestamp: i64| json!({"name": name, "email": email, "date": chrono::DateTime::from_timestamp(timestamp, 0)});
    json!({
        "sha": commit.sha, "node_id": node_id::git_object("C", repo_id, &commit.sha),
        "url": format!("{url}/git/commits/{}", commit.sha),
        "html_url": format!("{html}/commit/{}", commit.sha),
        "message": commit.message,
        "tree": {"sha": commit.tree_sha, "url": format!("{url}/git/trees/{}", commit.tree_sha)},
        "author": signature(&commit.author_name, &commit.author_email, commit.timestamp),
        "committer": signature(&commit.committer_name, &commit.committer_email, commit.committer_timestamp),
        "parents": commit.parents.iter().map(|sha| json!({
            "sha": sha, "url": format!("{url}/git/commits/{sha}"), "html_url": format!("{html}/commit/{sha}")
        })).collect::<Vec<_>>(),
        "verification": git_verification(commit.signature.as_deref(), commit.signature_payload.as_deref()),
    })
}

/// GitHub's commit object: the Git commit `detail` with its comment count, and the accounts
/// its author and committer emails belong to.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one part of GitHub's commit object."
)]
pub fn commit(
    commit: &CommitInfo,
    mut detail: Value,
    owner: &str,
    name: &str,
    repo_id: i64,
    comment_count: i64,
    author: Option<&Person>,
    committer: Option<&Person>,
) -> Value {
    // The Git object's parents link to Git commits; the commit object's link to commits.
    detail["parents"].take();
    detail["comment_count"] = json!(comment_count);
    let (url, html) = (repo_url(owner, name), repository_url(owner, name));
    let parents: Vec<_> = commit.parents.iter().map(|sha| json!({
        "sha": sha, "url": format!("{url}/commits/{sha}"), "html_url": format!("{html}/commit/{sha}")
    })).collect();
    json!({
        "sha": commit.sha, "node_id": node_id::git_object("C", repo_id, &commit.sha),
        "url": format!("{url}/commits/{}", commit.sha),
        "html_url": detail["html_url"],
        "comments_url": format!("{url}/commits/{}/comments", commit.sha),
        "commit": detail, "author": author.map(person),
        "committer": committer.map(person), "parents": parents,
    })
}

/// A `status` event. `commit` is the commit object, and `branches` the branches whose tip it is.
pub fn status(
    status: &codeyard_db_core::models::CommitStatus,
    commit: Value,
    branches: &[String],
    scope: Scope<'_>,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    let api = repo_url(owner, &repo.name);
    let sender = person(sender);
    json!({
        "id": status.id, "sha": status.sha, "name": format!("{owner}/{}", repo.name),
        "target_url": status.target_url, "context": status.context,
        "description": status.description, "state": status.state,
        "avatar_url": sender["avatar_url"], "commit": commit,
        "branches": branches.iter().map(|branch| json!({
            "name": branch,
            "commit": {"sha": status.sha, "url": format!("{api}/commits/{}", status.sha)},
            "protected": false,
        })).collect::<Vec<_>>(),
        "created_at": time(status.created_at), "updated_at": time(status.updated_at),
        "repository": repository(repo, owner), "sender": sender,
    })
}

pub fn reaction_rollup(
    url: &str,
    counts: &[crate::backend::db::reactions::ReactionCount],
) -> Value {
    let mut value =
        json!({"url":url,"total_count":counts.iter().map(|count|count.count).sum::<i64>()});
    for content in crate::backend::db::reactions::CONTENTS {
        value[content] = json!(counts
            .iter()
            .find(|count| count.content == content)
            .map_or(0, |count| count.count));
    }
    value
}

pub fn reaction(
    reaction: &crate::backend::db::reactions::Reaction,
    author: Option<&User>,
) -> Value {
    json!({
        "id": reaction.id,
        "node_id": crate::backend::api::github::node_id::encode("REA", reaction.id),
        "user": author.map(user),
        "content": reaction.content,
        "created_at": time(reaction.created_at),
    })
}

pub fn milestone(
    summary: &codeyard_db_core::models::MilestoneSummary,
    creator: Option<&User>,
    owner: &str,
    repo: &str,
) -> Value {
    let milestone = &summary.milestone;
    let url = format!(
        "{}/api/v3/repos/{}/{}/milestones/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(repo),
        milestone.number
    );
    json!({
        "id": milestone.id,
        "node_id": crate::backend::api::github::node_id::encode("M", milestone.id),
        "number": milestone.number,
        "url": url,
        "html_url": format!("{}/{}/{}/milestones/{}", link_origin(), codeyard_webui::encode_component(owner), codeyard_webui::encode_component(repo), milestone.number),
        "labels_url": format!("{url}/labels"),
        "title": milestone.title,
        "description": milestone.description,
        "state": milestone.state,
        "creator": creator.map(user),
        "open_issues": summary.open_items,
        "closed_issues": summary.closed_items,
        "created_at": time(milestone.created_at),
        "updated_at": time(milestone.updated_at),
        "closed_at": optional_time(milestone.closed_at),
        "due_on": optional_time(milestone.due_on),
    })
}

/// Labels use the same object in REST and issue webhook payloads.
pub fn label(label: &codeyard_db_core::models::Label, owner: &str, repo: &str) -> Value {
    json!({
        "id": label.id,
        "node_id": crate::backend::api::github::node_id::encode("L", label.id),
        "url": format!("{}/api/v3/repos/{}/{}/labels/{}", link_origin(), codeyard_webui::encode_component(owner), codeyard_webui::encode_component(repo), codeyard_webui::encode_component(&label.name)),
        "name": label.name,
        "color": label.color.trim_start_matches('#'),
        "description": label.description,
        "default": false,
        "archived_at": null,
        "archived_by": null,
    })
}

pub fn issues(action: &str, issue: &Issue, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "issue": issue_json(issue, author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn issue_suggestion(
    s: &crate::backend::db::suggestions::Suggestion,
) -> serde_json::Result<Value> {
    let target_value: Value = serde_json::from_str(&s.value_json)?;
    Ok(
        json!({"id":s.id,"issue_id":s.issue_id,"action":s.action,"state":s.state,"target_id":s.target_id,"target_value":target_value,"rationale":s.rationale,"confidence":s.confidence,"actor_id":s.actor_id,"issue_event_id":s.issue_event_id,"resolved_by":s.resolved_by,"created_at":time(s.created_at),"updated_at":time(s.updated_at)}),
    )
}

pub fn issue_type(attribute: &crate::backend::db::issue_attributes::Attribute) -> Value {
    json!({"id":attribute.id,"node_id":node_id::encode("IT",attribute.id),"name":attribute.name,"description":attribute.description,"color":attribute.color,"is_enabled":attribute.is_enabled,"created_at":time(attribute.created_at),"updated_at":time(attribute.updated_at)})
}
pub fn issue_field(attribute: &crate::backend::db::issue_attributes::Attribute) -> Value {
    let options=matches!(attribute.data_type.as_deref(),Some("single_select"|"multi_select")).then(||attribute.options.iter().map(|option|json!({"id":option.id,"name":option.name,"description":option.description,"color":option.color,"priority":option.priority})).collect::<Vec<_>>());
    json!({"id":attribute.id,"node_id":node_id::encode("IF",attribute.id),"name":attribute.name,"description":attribute.description,"data_type":attribute.data_type,"visibility":attribute.visibility,"options":options,"created_at":time(attribute.created_at),"updated_at":time(attribute.updated_at)})
}
pub fn issue_field_value(field: &crate::backend::db::issue_attributes::FieldValue) -> Value {
    let attribute = &field.attribute;
    let option = |id: i64| {
        attribute
            .options
            .iter()
            .find(|o| o.id == id)
            .map(|o| json!({"id":o.id,"name":o.name,"color":o.color}))
    };
    let single = if attribute.data_type.as_deref() == Some("single_select") {
        field.value.as_i64().and_then(option)
    } else {
        None
    };
    let multi = if attribute.data_type.as_deref() == Some("multi_select") {
        Some(
            field
                .value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_i64().and_then(option))
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let value = if multi.is_some() {
        Value::Null
    } else if let Some(single) = &single {
        single["name"].clone()
    } else {
        field.value.clone()
    };
    json!({"issue_field_id":attribute.id,"issue_field_name":attribute.name,"node_id":node_id::encode("IFV",field.id),"data_type":attribute.data_type,"value":value,"single_select_option":single,"multi_select_options":multi})
}

pub fn issue_event(
    event: &crate::backend::db::timeline::Event,
    actor: Option<&User>,
    repo: &Repository,
    owner: &str,
) -> Value {
    let mut value = json!({
        "id": event.id, "node_id": node_id::encode("IE", event.id),
        "url": format!("{}/issues/events/{}", crate::backend::api::github::repo_url(owner, &repo.name), event.id),
        "actor": actor.map(user), "event": event.kind,
        "commit_id": null, "commit_url": null, "created_at": time(event.created_at),
        "performed_via_github_app": null,
        "lock_reason": if event.kind == "locked" { Some(event.detail.as_str()).filter(|reason| !reason.is_empty()) } else { None },
    });
    if matches!(
        event.kind.as_str(),
        "issue_type_added" | "issue_type_changed"
    ) {
        value["issue_type"] = serde_json::from_str(&event.detail).unwrap_or(Value::Null);
    }
    if matches!(
        event.kind.as_str(),
        "issue_type_removed" | "issue_type_changed"
    ) {
        value["prev_issue_type"] = serde_json::from_str(&event.previous).unwrap_or(Value::Null);
    }
    value
}

/// The native comment tables have separate sequences; REST IDs keep them distinct.
pub fn conversation_comment_id(id: i64, pull_request: bool) -> i64 {
    id * 2 + i64::from(pull_request)
}

pub fn conversation_comment(
    id: i64,
    body: &str,
    author: &Person,
    dates: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    scope: Scope,
    number: i32,
    pull_request: bool,
) -> Value {
    let base = format!(
        "{}/api/v3/repos/{}/{}",
        link_origin(),
        codeyard_webui::encode_component(scope.owner),
        codeyard_webui::encode_component(&scope.repo.name)
    );
    let comment_id = conversation_comment_id(id, pull_request);
    json!({
        "id": comment_id,
        "node_id": crate::backend::api::github::node_id::encode("IC", comment_id),
        "url": format!("{base}/issues/comments/{comment_id}"),
        "issue_url": format!("{base}/issues/{number}"),
        "html_url": format!("{}/{}/{}?comment={id}#comment-{id}", repository_url(scope.owner, &scope.repo.name), if pull_request { "pull" } else { "issues" }, number),
        "user": person(author),
        "body": body,
        "created_at": time(dates.0),
        "updated_at": time(dates.1),
    })
}

pub fn issue_comment(
    action: &str,
    previous_body: Option<&str>,
    comment: &IssueComment,
    issue: &Issue,
    issue_author: &Person,
    comment_author: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    let mut payload = json!({
        "action": action,
        "issue": issue_json(issue, issue_author, repo, owner),
        "comment": ({
            let mut value = conversation_comment(comment.id, &comment.body, comment_author, (comment.created_at, comment.updated_at), scope, issue.number, false);
            value["html_url"] = json!(issue_comment_url(action, comment, issue, repo, owner));
            value
        }),
        "repository": repository(repo, owner),
        "sender": person(sender),
    });
    if let Some(body) = previous_body {
        payload["changes"] = json!({"body":{"from":body}});
    }
    payload
}

/// The issue page resolves `?comment=` to the page that holds the comment. A deleted comment
/// has no page, so it links to the issue.
fn issue_comment_url(
    action: &str,
    comment: &IssueComment,
    issue: &Issue,
    repo: &Repository,
    owner: &str,
) -> String {
    let base = format!(
        "{}/issues/{}",
        repository_url(owner, &repo.name),
        issue.number
    );
    if action == "deleted" {
        base
    } else {
        format!("{base}?comment={0}#comment-{0}", comment.id)
    }
}

pub fn issue_json(issue: &Issue, author: &Person, repo: &Repository, owner: &str) -> Value {
    let base = format!(
        "{}/api/v3/repos/{}/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(&repo.name)
    );
    let url = format!("{base}/issues/{}", issue.number);
    json!({
        "url": url,
        "node_id": crate::backend::api::github::node_id::encode("I", issue.id),
        "repository_url": base,
        "labels_url": format!("{url}/labels{{/name}}"),
        "comments_url": format!("{url}/comments"),
        "events_url": format!("{url}/events"),
        "active_lock_reason": issue.lock_reason,
        "id": issue.id,
        "number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "state": issue.state,
        "state_reason": issue.state_reason,
        "author_association": if repo.is_personal_owner(author.id) { "OWNER" } else { "NONE" },
        "locked": issue.locked,
        "user": person(author),
        "html_url": format!("{}/issues/{}", repository_url(owner, &repo.name), issue.number),
        "created_at": time(issue.created_at),
        "updated_at": time(issue.updated_at),
        "closed_at": optional_time(issue.closed_at),
    })
}

pub fn pull_request(action: &str, pull: &PullRequest, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "number": pull.number,
        "pull_request": pull_request_json(pull, author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn comment_pin(pin: &crate::backend::db::pins::Pin, pinner: Option<&User>) -> Value {
    json!({"pinned_at":time(pin.pinned_at),"pinned_by":pinner.map(user)})
}

pub fn pull_request_issue(
    pull: &PullRequest,
    author: &Person,
    repo: &Repository,
    owner: &str,
) -> Value {
    let mut value = pull_request_json(pull, author, repo, owner);
    let url = value["issue_url"].clone();
    value["pull_request"] = json!({
        "url": value["url"], "html_url": value["html_url"],
        "diff_url": value["diff_url"], "patch_url": value["patch_url"], "merged_at": value["merged_at"],
    });
    value["url"] = url;
    value["repository_url"] = json!(crate::backend::api::github::repo_url(owner, &repo.name));
    value["labels_url"] = json!(format!(
        "{}/labels{{/name}}",
        value["url"].as_str().expect("issue URL")
    ));
    value["events_url"] = json!(format!(
        "{}/events",
        value["url"].as_str().expect("issue URL")
    ));
    value["state_reason"] = Value::Null;
    value["active_lock_reason"] = json!(pull.lock_reason);
    value
}

pub fn pull_request_json(
    pull: &PullRequest,
    author: &Person,
    repo: &Repository,
    owner: &str,
) -> Value {
    let repo_url = repo_url(owner, &repo.name);
    let url = format!("{repo_url}/pulls/{}", pull.number);
    let html_url = format!("{}/pull/{}", repository_url(owner, &repo.name), pull.number);
    let mut value = json!({
        "url": url, "node_id": node_id::encode("PR", pull.id),"maintainer_can_modify":pull.maintainer_can_modify,
        "locked": pull.locked, "active_lock_reason": pull.lock_reason, "assignee": null, "assignees": [], "labels": [], "milestone": null,
        "requested_reviewers": [], "requested_teams": [], "auto_merge": null, "maintainer_can_modify": false,
        "author_association": if repo.personal_owner_id() == Some(author.id) { "OWNER" } else { "NONE" },
        "diff_url": format!("{html_url}.diff"), "patch_url": format!("{html_url}.patch"),
        "issue_url": format!("{repo_url}/issues/{}", pull.number),
        "commits_url": format!("{url}/commits"), "comments_url": format!("{repo_url}/issues/{}/comments", pull.number),
        "review_comments_url": format!("{url}/comments"), "review_comment_url": format!("{repo_url}/pulls/comments{{/number}}"),
        "statuses_url": format!("{repo_url}/statuses/{}", pull.head_sha),
        "id": pull.id,
        "number": pull.number,
        "title": pull.title,
        "body": pull.body,
        "state": if pull.merged || pull.state == "merged" { "closed" } else { &pull.state },
        "draft": pull.is_draft,
        "maintainer_can_modify": pull.maintainer_edits_enabled(),
        "merged": pull.merged,
        "mergeable": pull.mergeable,
        "mergeable_state": if pull.merged { "clean" } else { "unknown" },
        "merged_at": optional_time(pull.merged_at),
        "merge_commit_sha": pull.merge_commit_sha,
        "user": person(author),
        "head": { "ref": pull.head_branch, "sha": pull.head_sha, "repo": if pull.head_repo_id==Some(repo.id) {repository(repo,owner)} else {Value::Null} },
        "base": { "ref": pull.base_branch, "sha": pull.base_sha, "repo":repository(repo,owner) },
        "html_url": format!("{}/pull/{}", repository_url(owner, &repo.name), pull.number),
        "created_at": time(pull.created_at),
        "updated_at": time(pull.updated_at),
        "closed_at": optional_time(pull.closed_at),
    });
    for (name, field) in [
        ("self", "url"),
        ("html", "html_url"),
        ("issue", "issue_url"),
        ("comments", "comments_url"),
        ("commits", "commits_url"),
        ("review_comments", "review_comments_url"),
        ("review_comment", "review_comment_url"),
        ("statuses", "statuses_url"),
    ] {
        value["_links"][name] = json!({"href": value[field]});
    }
    value
}

/// GitHub's `auto_merge` object for a pull request's saved auto-merge request.
pub fn auto_merge(
    request: &crate::backend::db::pull_requests::auto_merge::AutoMergeRequest,
    enabled_by: Option<&User>,
) -> Value {
    json!({
        "enabled_by": enabled_by.map_or(Value::Null, user),
        "merge_method": request.merge_method.to_ascii_lowercase(),
        "commit_title": request.commit_title.as_deref().unwrap_or(""),
        "commit_message": request.commit_message.as_deref().unwrap_or(""),
    })
}

pub fn pull_request_review_json(
    review: &PullRequestReview,
    reviewer: &Person,
    pull: &PullRequest,
    repo: &Repository,
    owner: &str,
) -> Value {
    let url = format!(
        "{}/api/v3/repos/{}/{}/pulls/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(&repo.name),
        pull.number
    );
    let html_url = format!(
        "{}/pulls/{}?review={id}#review-{id}",
        repository_url(owner, &repo.name),
        pull.number,
        id = review.id
    );
    let mut value = json!({
        "id": review.id, "node_id": node_id::encode("PRR", review.id), "user": person(reviewer),
        "body": review.body.as_deref().unwrap_or(""), "state": review.state.to_ascii_uppercase(),
        "commit_id": review.commit_sha, "html_url": html_url, "pull_request_url": url,
        "author_association": if repo.is_personal_owner(reviewer.id) { "OWNER" } else { "NONE" },
        "_links": {"html": {"href": html_url}, "pull_request": {"href": url}},
    });
    if review.state != "pending" {
        value["submitted_at"] = time(review.created_at);
    }
    value
}

pub fn pull_request_review(
    action: &str,
    review: &PullRequestReview,
    pull: &PullRequest,
    pull_author: &Person,
    reviewer: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    // GitHub spells the state in lower case, and names a dismissed review "dismissed".
    let mut review_value = pull_request_review_json(review, reviewer, pull, repo, owner);
    review_value["state"] = json!(review.state.to_ascii_lowercase());
    json!({
        "action": action,
        "review": review_value,
        "pull_request": pull_request_json(pull, pull_author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn pull_request_review_comment_json(
    comment: &PullRequestComment,
    pull: &PullRequest,
    author: &Person,
    repo: &Repository,
    owner: &str,
) -> Value {
    let root = format!(
        "{}/api/v3/repos/{}/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(&repo.name)
    );
    let url = format!("{root}/pulls/comments/{}", comment.id);
    let pull_url = format!("{root}/pulls/{}", pull.number);
    let html = format!(
        "{}/pull/{}?comment={id}#comment-{id}",
        repository_url(owner, &repo.name),
        pull.number,
        id = comment.id
    );
    let current = review_comment_is_current(comment, pull);
    // GitHub sends the range and position fields as null rather than leaving them out;
    // `review_comment_diff` fills the positions from the diff.
    let mut value = json!({"id":comment.id,"node_id":node_id::encode("PRRC",comment.id),"url":url,"html_url":html,"pull_request_url":pull_url,
        "pull_request_review_id":comment.review_id,"body":comment.body,"path":comment.path,"diff_hunk":"",
        "commit_id":comment.commit_sha.as_deref().unwrap_or(&pull.head_sha),"original_commit_id":comment.commit_sha.as_deref().unwrap_or(&pull.head_sha),
        "user":person(author),"created_at":time(comment.created_at),"updated_at":time(comment.updated_at),"author_association":if repo.is_personal_owner(author.id){"OWNER"}else{"NONE"},
        "subject_type":if comment.line.is_some(){"line"}else{"file"},
        "line":comment.line.filter(|_| current),"original_line":comment.line,"side":comment.side,
        "start_line":comment.start_line.filter(|_| current),"original_start_line":comment.start_line,
        "start_side":comment.start_line.and(comment.start_side.as_deref()),
        "position":null,"original_position":null,
        "_links":{"self":{"href":url},"html":{"href":html},"pull_request":{"href":pull_url}}});
    if let Some(parent) = comment.in_reply_to_comment_id {
        value["in_reply_to_id"] = json!(parent);
    }
    value
}

/// Whether a review comment was written on the pull request's current head. An outdated
/// comment keeps only its original coordinates.
pub fn review_comment_is_current(comment: &PullRequestComment, pull: &PullRequest) -> bool {
    comment
        .commit_sha
        .as_deref()
        .is_none_or(|sha| sha == pull.head_sha)
}

/// The revision pair whose diff a review comment was written on. A comment keeps the base it
/// was written against.
pub fn review_comment_revisions<'a>(
    comment: &'a PullRequestComment,
    pull: &'a PullRequest,
) -> (&'a str, &'a str) {
    (
        comment
            .diff_base_sha
            .as_deref()
            .unwrap_or_else(|| pull.diff_base()),
        comment.commit_sha.as_deref().unwrap_or(&pull.head_sha),
    )
}

/// Fills a review comment's `diff_hunk` and positions from `files`, the diff of
/// `review_comment_revisions`. An outdated comment has no `position` in the current diff.
pub fn review_comment_diff(
    value: &mut Value,
    comment: &PullRequestComment,
    pull: &PullRequest,
    files: &[crate::backend::repositories::store::DiffFileInfo],
) {
    let Some(file) = files
        .iter()
        .find(|file| Some(file.filename.as_str()) == comment.path.as_deref())
    else {
        return;
    };
    value["diff_hunk"] = json!(file.patch);
    let Some(line) = comment.line else {
        return;
    };
    let left = comment.side.as_deref() == Some("LEFT");
    if let Some((position, _, _)) = diff_positions(&file.patch)
        .into_iter()
        .find(|(_, old, new)| {
            if left {
                *old == Some(line)
            } else {
                *new == Some(line)
            }
        })
    {
        value["original_position"] = json!(position);
        if review_comment_is_current(comment, pull) {
            value["position"] = json!(position);
        }
    }
}

/// Each diff row's position with its old and new line. Positions count diff rows, not file
/// lines; later hunk headers count too.
pub fn diff_positions(patch: &str) -> Vec<(i32, Option<i32>, Option<i32>)> {
    let (mut old, mut new, mut pos, mut begun) = (0, 0, 0, false);
    let mut rows = Vec::new();
    for row in patch.lines() {
        if row.starts_with("@@ ") {
            let mut fields = row.split_whitespace();
            fields.next();
            old = fields
                .next()
                .and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok())
                .unwrap_or(0);
            new = fields
                .next()
                .and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok())
                .unwrap_or(0);
            if begun {
                pos += 1;
            }
            begun = true;
            continue;
        }
        if !begun {
            continue;
        }
        pos += 1;
        match row.as_bytes().first() {
            Some(b'+') => {
                rows.push((pos, None, Some(new)));
                new += 1;
            }
            Some(b'-') => {
                rows.push((pos, Some(old), None));
                old += 1;
            }
            Some(b' ') => {
                rows.push((pos, Some(old), Some(new)));
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
    rows
}

pub fn pull_request_review_comment(
    action: &str,
    comment: &PullRequestComment,
    pull: &PullRequest,
    pull_author: &Person,
    comment_author: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "comment": pull_request_review_comment_json(comment, pull, comment_author, repo, owner),
        "pull_request": pull_request_json(pull, pull_author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

/// A conversation comment on a pull request. GitHub sends it as an issue_comment whose issue is
/// the pull request. `link` is the comment link, such as `?comment=7#comment-7`.
pub fn pull_request_conversation_comment(
    action: &str,
    comment: &PullRequestComment,
    pull: &PullRequest,
    pull_author: &Person,
    comment_author: &Person,
    link: &str,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    // GitHub marks an issue that is a pull request with a `pull_request` link object.
    let mut issue = pull_request_json(pull, pull_author, repo, owner);
    issue["pull_request"] = json!({ "html_url": issue["html_url"].clone() });
    json!({
        "action": action,
        "issue": issue,
        "comment": ({
            let mut value = conversation_comment(comment.id, &comment.body, comment_author, (comment.created_at, comment.updated_at), scope, pull.number, true);
            value["html_url"] = json!(format!("{}/pull/{}{link}", repository_url(owner, &repo.name), pull.number));
            value
        }),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn release_asset(
    asset: &ReleaseAsset,
    release: &Release,
    owner: &str,
    name: &str,
    uploader: Option<Value>,
    digest: &str,
) -> Value {
    let base = crate::backend::api::github::repo_url(owner, name);
    json!({
        "id": asset.id, "node_id": crate::backend::api::github::node_id::encode("RA", asset.id),
        "url": format!("{base}/releases/assets/{}", asset.id),
        "browser_download_url": format!("{}/{}/{}/releases/download/{}/{}", link_origin(),
            codeyard_webui::encode_component(owner), codeyard_webui::encode_component(name),
            codeyard_webui::encode_component(&release.tag_name), codeyard_webui::encode_component(&asset.name)),
        "name": asset.name, "label": asset.label, "state": "uploaded",
        "content_type": asset.content_type, "size": asset.size, "digest": format!("sha256:{digest}"),
        "download_count": asset.download_count, "uploader": uploader,
        "created_at": time(asset.created_at), "updated_at": time(asset.updated_at),
    })
}

pub fn release_json(
    release: &Release,
    author: &Person,
    owner: &str,
    name: &str,
    assets: Option<Vec<Value>>,
) -> Value {
    let base = crate::backend::api::github::repo_url(owner, name);
    let id = release.id;
    let mut value = json!({
        "id": id, "node_id": crate::backend::api::github::node_id::encode("RE", id),
        "url": format!("{base}/releases/{id}"), "assets_url": format!("{base}/releases/{id}/assets"),
        "upload_url": format!("{base}/releases/{id}/assets{{?name,label}}"),
        "html_url": format!("{}/{}/{}/releases/tag/{}", link_origin(), codeyard_webui::encode_component(owner),
            codeyard_webui::encode_component(name), codeyard_webui::encode_component(&release.tag_name)),
        "tarball_url": format!("{base}/tarball/{}", codeyard_webui::encode_component(&format!("refs/tags/{}", release.tag_name))),
        "zipball_url": format!("{base}/zipball/{}", codeyard_webui::encode_component(&format!("refs/tags/{}", release.tag_name))),
        "tag_name": release.tag_name, "target_commitish": release.target_commitish,
        "name": release.name, "body": release.body, "draft": release.is_draft, "prerelease": release.is_prerelease,"immutable":release.is_immutable,
        "author": simple_person(author), "created_at": time(release.created_at),
        "published_at": optional_time(release.published_at),
        // Release edits are not timestamped.
        "updated_at": null,
    });
    if let Some(assets) = assets {
        value["assets"] = json!(assets);
    }
    value
}

/// GitHub's `changes` of an `edited` release event: the previous value of each changed field.
pub fn release_changes(before: &Release, after: &Release) -> Value {
    let mut changes = json!({});
    if before.name != after.name {
        changes["name"] = json!({"from": before.name.as_deref().unwrap_or("")});
    }
    if before.body != after.body {
        changes["body"] = json!({"from": before.body.as_deref().unwrap_or("")});
    }
    if before.tag_name != after.tag_name {
        changes["tag_name"] = json!({"from": before.tag_name});
    }
    if before.is_latest != after.is_latest {
        changes["make_latest"] = json!({"to": after.is_latest});
    }
    changes
}

pub fn release(action: &str, release: &Release, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "release": release_json(release, author, owner, &repo.name, None),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

/// Repository-scoped native inbox subjects use the same repository document as hooks.
pub fn notification_thread_json(
    view: &crate::backend::db::notifications::threads::ThreadView,
) -> Value {
    let url =
        crate::backend::api::github::api_url(&format!("/notifications/threads/{}", view.thread.id));
    json!({"id":view.thread.id.to_string(),"repository":repository(&view.repository,&view.owner),"subject":{"title":view.title,"url":view.subject_url,"latest_comment_url":view.latest_comment_url,"type":view.subject_type},"reason":view.reason,"unread":view.unread,"updated_at":view.updated_at,"last_read_at":view.last_read_at,"url":url,"subscription_url":format!("{url}/subscription")})
}
pub fn notification_subscription_json(
    thread: &crate::backend::db::notifications::threads::NotificationThread,
    subscription: &crate::backend::db::notifications::threads::ThreadSubscription,
) -> Value {
    let url =
        crate::backend::api::github::api_url(&format!("/notifications/threads/{}", thread.id));
    json!({"subscribed":subscription.subscribed,"ignored":subscription.ignored,"reason":null,"created_at":subscription.created_at,"url":format!("{url}/subscription"),"thread_url":url})
}

/// Persisted activity uses the current repository and actor documents.
pub fn event_json(
    event: &crate::backend::db::events::Event,
    repo: &Repository,
    owner: &str,
    actor: Value,
) -> Value {
    let mut payload = event.payload.clone();
    if let Some(object) = payload.as_object_mut() {
        for key in [
            "repository",
            "sender",
            "organization",
            "installation",
            "enterprise",
        ] {
            object.remove(key);
        }
    }
    if event.kind == "WatchEvent" {
        payload["action"] = json!("started");
    }
    if event.kind == "PushEvent" {
        payload["repository_id"] = json!(repo.id);
        payload["push_id"] = json!(event.id);
        let head = payload["after"].take();
        payload["head"] = head;
        if let Some(object) = payload.as_object_mut() {
            object.remove("after");
        }
    }
    let mut value = json!({"id":event.id.to_string(),"type":event.kind,"actor":actor,"repo":{"id":repo.id,"name":format!("{owner}/{}",repo.name),"url":crate::backend::api::github::repo_url(owner,&repo.name)},"payload":payload,"public":event.is_public && !repo.is_private,"created_at":event.created_at});
    if repo.owner_org_id.is_some() {
        value["org"] = repository_owner(repo, owner);
    }
    value
}

pub fn ref_change(ref_type: &str, name: &str, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "ref": name,
        "ref_type": ref_type,
        "full_ref": match ref_type {"branch"=>json!(format!("refs/heads/{name}")),"tag"=>json!(format!("refs/tags/{name}")),_=>Value::Null},
        "master_branch":repo.default_branch,"description":repo.description,"pusher_type":"user",
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn fork(
    forkee: &Repository,
    forker: &Person,
    repo: &Repository,
    owner: &str,
    forkee_owner: &str,
) -> Value {
    json!({
        // Source-scoped deliveries do not confer access to a private destination owner.
        "forkee": if forkee.is_private { Value::Null } else { repository(forkee, forkee_owner) },
        "repository": repository(repo, owner),
        "sender": person(forker),
    })
}

/// `created` or `deleted`, for a star. Starred and unstarred follow GitHub's wording, and a
/// removed star has no `starred_at`.
pub fn star(action: &str, starrer: &Person, repo: &Repository, owner: &str) -> Value {
    json!({
        "action": action,
        "starred_at": (action != "deleted").then(|| chrono::Utc::now().to_rfc3339()),
        "repository": repository(repo, owner),
        "sender": person(starrer),
    })
}

/// One commit of a push, as GitHub lists it.
#[derive(Debug, Clone)]
pub struct PushedCommit {
    pub id: String,
    pub tree_id: String,
    pub message: String,
    pub time: i64,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: i64,
    /// No ref held the commit before this push.
    pub distinct: bool,
    /// The paths the commit changed against its first parent; `None` when they were not read.
    pub files: Option<ChangedFiles>,
}

/// GitHub's `added`, `removed` and `modified` path lists of one pushed commit.
#[derive(Debug, Default, Clone)]
pub struct ChangedFiles {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
}

impl ChangedFiles {
    pub fn bytes(&self) -> usize {
        self.added
            .iter()
            .chain(&self.removed)
            .chain(&self.modified)
            .map(|path| std::mem::size_of::<String>() + path.len())
            .sum()
    }
}

/// Paths listed for one pushed commit; a larger commit lists only its first paths.
const MAX_PUSHED_COMMIT_FILES: usize = 3000;
/// Wall-clock budget for reading the changed paths of one push's commits.
const PUSHED_FILES_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Default, Clone)]
pub struct PushedCommits {
    pub commits: Vec<PushedCommit>,
    pub head: Option<PushedCommit>,
    /// The new tip does not contain the old one.
    pub forced: bool,
}

pub(crate) fn bounded_text(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn pushed_commit(
    repository: &git2::Repository,
    revision: &str,
    distinct: &dyn Fn(&str) -> bool,
    files_until: std::time::Instant,
) -> Option<PushedCommit> {
    let commit =
        crate::backend::git::repository::peel_to_commit_checked(repository, revision).ok()?;
    let (author, committer) = (commit.author(), commit.committer());
    let id = commit.id().to_string();
    let files = (std::time::Instant::now() < files_until)
        .then(|| changed_files(repository, &commit))
        .flatten();
    Some(PushedCommit {
        distinct: distinct(&id),
        files,
        tree_id: commit.tree_id().to_string(),
        message: bounded_text(commit.message().unwrap_or(""), 4096),
        time: author.when().seconds(),
        author_name: bounded_text(author.name().unwrap_or(""), 256),
        author_email: bounded_text(author.email().unwrap_or(""), 256),
        committer_name: bounded_text(committer.name().unwrap_or(""), 256),
        committer_email: bounded_text(committer.email().unwrap_or(""), 256),
        committer_time: committer.when().seconds(),
        id,
    })
}

/// The paths a commit changed against its first parent, or against nothing for a root
/// commit, like the commit page shows them. Renames are listed as a removal and an addition.
fn changed_files(repository: &git2::Repository, commit: &git2::Commit<'_>) -> Option<ChangedFiles> {
    fn path(file: git2::DiffFile<'_>) -> Option<String> {
        file.path_bytes()
            .map(|path| String::from_utf8_lossy(path).into_owned())
    }
    let tree = commit.tree().ok()?;
    let parent = match commit.parent_count() {
        0 => None,
        _ => Some(commit.parent(0).ok()?.tree().ok()?),
    };
    let mut options = git2::DiffOptions::new();
    options.skip_binary_check(true);
    let diff = repository
        .diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))
        .ok()?;
    let mut files = ChangedFiles::default();
    for delta in diff.deltas().take(MAX_PUSHED_COMMIT_FILES) {
        match delta.status() {
            git2::Delta::Added => files.added.extend(path(delta.new_file())),
            git2::Delta::Deleted => files.removed.extend(path(delta.old_file())),
            git2::Delta::Modified | git2::Delta::Typechange => {
                files.modified.extend(path(delta.new_file()))
            }
            _ => {}
        }
    }
    Some(files)
}

/// The newest 2048 commits reachable from `after` and from none of `excluded`, oldest first
/// when `reverse` is set. Git's bounded subprocess avoids libgit2's eager hidden-range walk.
async fn rev_list<'a>(
    path: &Path,
    after: &str,
    excluded: impl Iterator<Item = &'a str>,
    reverse: bool,
) -> anyhow::Result<Vec<String>> {
    use crate::backend::git::repository::process;
    let mut command = process::git_command()?;
    command
        .current_dir(path)
        .args(["rev-list", "--max-count=2048", "--stdin"]);
    if reverse {
        command.arg("--reverse");
    }
    let mut input = format!("{after}\n");
    for id in excluded {
        input.push('^');
        input.push_str(id);
        input.push('\n');
    }
    let (status, output) = process::buffered(
        command,
        input.as_bytes(),
        2048 * 65,
        std::time::Duration::from_secs(2),
    )
    .await?;
    anyhow::ensure!(status.success(), "Webhook commit listing failed");
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .take(2048)
        .map(str::to_owned)
        .collect())
}

/// At most 2048 introduced commits, with a separate new head even for a rewind. `previous`
/// holds the tips every ref had before the push: a new ref lists only commits none of them
/// reached, and a listed commit is distinct when none of them reached it. Without it, a new
/// ref lists its history and every listed commit counts as distinct. Called only on a
/// blocking thread.
pub fn pushed_commits(
    path: &Path,
    before: Option<&str>,
    after: &str,
    previous: Option<&[String]>,
) -> anyhow::Result<PushedCommits> {
    use crate::backend::git::repository::process;
    let repository = git2::Repository::open_bare(path)?;
    let previous_tips = || previous.into_iter().flatten().map(String::as_str);
    let collect = async {
        match before {
            Some(before) => rev_list(path, after, std::iter::once(before), true).await,
            None => rev_list(path, after, previous_tips(), true).await,
        }
    };
    // For a moved ref, its own range may hold commits another ref already had.
    let distinct = async {
        match (before, previous) {
            (Some(_), Some(_)) => rev_list(path, after, previous_tips(), false)
                .await
                .map(|ids| Some(ids.into_iter().collect::<std::collections::HashSet<_>>())),
            _ => Ok(None),
        }
    };
    // `merge-base --is-ancestor` exits 1 when the old tip is not in the new history.
    let forced = async {
        let Some(before) = before else {
            return Ok::<_, anyhow::Error>(false);
        };
        let mut command = process::git_command()?;
        command
            .current_dir(path)
            .args(["merge-base", "--is-ancestor", before, after]);
        let (status, _) =
            process::buffered(command, b"", 64, std::time::Duration::from_secs(2)).await?;
        match status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => anyhow::bail!("Webhook ancestry lookup failed"),
        }
    };
    let (listed, distinct, forced) = tokio::runtime::Handle::current().block_on(async {
        Ok::<_, anyhow::Error>((collect.await?, distinct.await?, forced.await?))
    })?;
    let files_until = std::time::Instant::now() + PUSHED_FILES_BUDGET;
    // Without a separate walk every listed commit is new; the head is new only when listed.
    let distinct = distinct.unwrap_or_else(|| listed.iter().cloned().collect());
    let is_distinct = |id: &str| distinct.contains(id);
    let head = pushed_commit(&repository, after, &is_distinct, files_until);
    let commits = listed
        .iter()
        .map(|oid| {
            pushed_commit(&repository, oid, &is_distinct, files_until)
                .ok_or_else(|| anyhow::anyhow!("Webhook commit lookup failed"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(PushedCommits {
        commits,
        head,
        forced,
    })
}

fn pushed_commit_json(commit: &PushedCommit, repo: &Repository, owner: &str) -> Value {
    let date = |seconds| chrono::DateTime::from_timestamp(seconds, 0).map(time);
    let mut value = json!({
        "id": commit.id,
        "tree_id": commit.tree_id,
        "distinct": commit.distinct,
        "message": commit.message,
        "timestamp": date(commit.time),
        "url": format!("{}/commit/{}", repository_url(owner, &repo.name), commit.id),
        "author": { "name": commit.author_name, "email": commit.author_email, "date": date(commit.time) },
        "committer": { "name": commit.committer_name, "email": commit.committer_email, "date": date(commit.committer_time) },
    });
    if let Some(files) = &commit.files {
        value["added"] = json!(files.added);
        value["removed"] = json!(files.removed);
        value["modified"] = json!(files.modified);
    }
    value
}

/// A branch or tag push. `before` is absent for a new ref; the caller sends `create` and
/// `delete` for the ref changes that have no commits to list.
pub fn push(
    reference: &str,
    before: &str,
    after: &str,
    commits: &PushedCommits,
    repo: &Repository,
    owner: &str,
    pusher: &Person,
) -> Value {
    let commit_values: Vec<Value> = commits
        .commits
        .iter()
        .map(|commit| pushed_commit_json(commit, repo, owner))
        .collect();
    let head = commits
        .head
        .as_ref()
        .map(|head| pushed_commit_json(head, repo, owner));
    let created = before.chars().all(|c| c == '0');
    let deleted = after.chars().all(|c| c == '0');
    let html = repository_url(owner, &repo.name);
    // A zero revision is not a commit the comparison page can open, so a created or deleted
    // ref links to the one commit that exists.
    let compare = if created {
        format!("{html}/commit/{after}")
    } else if deleted {
        format!("{html}/commit/{before}")
    } else {
        format!("{html}/compare/{before}...{after}")
    };
    json!({
        "ref": reference,
        "before": before,
        "after": after,
        "created": created,
        "deleted": deleted,
        "forced": commits.forced,
        // GitHub names a branch here only for a tag pushed at a branch tip.
        "base_ref": null,
        "compare": compare,
        "commits": commit_values,
        "head_commit": head,
        "repository": repository(repo, owner),
        "pusher": { "name": pusher.login, "id": pusher.id },
        "sender": person(pusher),
    })
}

pub fn action_workflow(
    workflow: &crate::backend::db::action_workflows::Workflow,
    owner: &str,
    repo: &str,
) -> Value {
    let url = format!(
        "{}/actions/workflows/{}",
        repo_url(owner, repo),
        workflow.id
    );
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("workflow", &workflow.path)
        .finish();
    json!({"id":workflow.id,"node_id":node_id::encode("WF",workflow.id),"name":workflow.name,"path":workflow.path,"state":workflow.state,"url":url,"html_url":format!("{}/actions?{query}",repository_url(owner,repo)),"badge_url":format!("{url}/badge.svg"),"created_at":time(workflow.created_at),"updated_at":time(workflow.updated_at)})
}

/// Artifact metadata uses the same durable object and run identifiers as runtime downloads.
/// A workflow execution document; related objects are projected by their shared builders.
pub fn workflow_run(action: &str, run: Value, workflow: Value, scope: Scope<'_>) -> Value {
    json!({"action":action, "workflow_run":run, "workflow":workflow,
        "repository":repository(scope.repo, scope.owner), "sender":person(scope.sender)})
}

pub fn workflow_job(action: &str, job: Value, scope: Scope<'_>) -> Value {
    json!({"action":action, "workflow_job":job,
        "repository":repository(scope.repo, scope.owner), "sender":person(scope.sender)})
}

/// `head_repository` is the pull request's head repository, which for a `pull_request_target`
/// run is not the repository the run checked out.
pub fn action_run_pull_requests(
    run: &codeyard_db_core::models::CiRun,
    pull: Option<&PullRequest>,
    repository: &Value,
    head_repository: &Value,
    exclude: bool,
) -> Value {
    if exclude {
        return json!([]);
    }
    let Some(pull) = pull else {
        // The association was deleted; an empty list would falsely claim there never was one.
        return if run.event == "pull_request" {
            Value::Null
        } else {
            json!([])
        };
    };
    let snapshot = run.pull_request_event.as_ref();
    let event_head = run
        .event_payload
        .as_ref()
        .map(|event| &event["pull_request"]["head"]);
    let head_sha = snapshot
        .and_then(|event| event["head_sha"].as_str())
        .or_else(|| event_head.and_then(|head| head["sha"].as_str()))
        .unwrap_or(if run.event == "pull_request_target" {
            &pull.head_sha
        } else {
            run.head_sha.as_deref().unwrap_or(&pull.head_sha)
        });
    let side = |reference: &str, sha: &str, repository: &Value| json!({"ref":reference,"sha":sha,"repo":{"id":repository["id"],"url":repository["url"],"name":repository["name"]}});
    json!([{"id":pull.id,"number":pull.number,"url":format!("{}/pulls/{}",repository["url"].as_str().unwrap_or(""),pull.number),
        "head":side(snapshot.and_then(|event|event["head_ref"].as_str()).unwrap_or(&pull.head_branch),head_sha,head_repository),
        "base":side(snapshot.and_then(|event|event["base_ref"].as_str()).unwrap_or(&pull.base_branch),snapshot.and_then(|event|event["base_sha"].as_str()).unwrap_or(&pull.base_sha),repository)}])
}

pub fn action_run(
    run: &codeyard_db_core::models::CiRun,
    owner: &str,
    repository: Value,
    head_repository: Value,
    head_commit: Value,
    metadata: &crate::backend::db::action_runs::RunMetadata,
    pull_requests: Value,
) -> Value {
    let workflow_id = metadata.workflow_id;
    let suite = metadata.check_suite_id;
    let name = repository["name"].as_str().unwrap_or("");
    let id = run.workflow_run_id.unwrap_or(run.id);
    let api = repo_url(owner, name);
    let url = format!("{api}/actions/runs/{id}");
    let branch = run.head_branch();
    let title = run
        .pull_request_event
        .as_ref()
        .and_then(|event| event["title"].as_str())
        .or_else(|| {
            head_commit["message"]
                .as_str()
                .and_then(|message| message.lines().next())
        })
        .unwrap_or(&run.workflow_path);
    let title = bounded_text(title, 1024);
    let (status, conclusion) = crate::backend::db::action_runs::run_state(run);
    let mut value = json!({
        "id":id,"node_id":node_id::encode("WR",id),"head_branch":(!branch.is_empty()).then_some(branch),
        "run_number":run.workflow_run_number,"run_attempt":run.workflow_run_attempt.unwrap_or(1),"display_title":title,
        "event":run.event,"status":status,"conclusion":conclusion,"head_sha":run.head_sha.as_deref().unwrap_or(""),
        "path":run.workflow_path,"workflow_id":workflow_id,"url":url,
        "html_url":format!("{}/{}/{}/actions?job={}",link_origin(),codeyard_webui::encode_component(owner),codeyard_webui::encode_component(name),run.run_id),
        "created_at":time(run.created_at),"updated_at":time(run.updated_at),
        "head_commit":head_commit,"head_repository":head_repository,"repository":repository,
        "jobs_url":format!("{url}/jobs"),"logs_url":format!("{url}/logs"),
        "check_suite_id":suite,"check_suite_node_id":suite.map(|id|node_id::encode("CSU",id)),
        "check_suite_url":suite.map(|id|format!("{api}/check-suites/{id}")),
        "cancel_url":format!("{url}/cancel"),"rerun_url":format!("{url}/rerun"),"artifacts_url":format!("{url}/artifacts"),
        "workflow_url":format!("{api}/actions/workflows/{workflow_id}"),"pull_requests":pull_requests,
    });
    if let Some(started) = run.started_at {
        value["run_started_at"] = time(started);
    }
    let attempt = run.workflow_run_attempt.unwrap_or(1);
    value["previous_attempt_url"] = if attempt > 1 {
        json!(format!("{url}/attempts/{}", attempt - 1))
    } else {
        Value::Null
    };
    value
}

pub fn action_job(
    job: &crate::backend::db::action_jobs::Job,
    steps: &[crate::backend::db::action_jobs::Step],
    run: &codeyard_db_core::models::CiRun,
    owner: &str,
    repo: &str,
) -> Value {
    let api = repo_url(owner, repo);
    let run_id = run.workflow_run_id.unwrap_or(run.id);
    let branch = run.head_branch();
    // The run page opens a job's logs by its name when no other job shares it.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("job", &run.run_id.to_string())
        .append_pair("view", "logs")
        .append_pair("workflow_job", &job.name)
        .finish();
    json!({"id":job.id,"node_id":node_id::encode("J",job.id),"run_id":run_id,"run_attempt":run.workflow_run_attempt.unwrap_or(1),
        "run_url":format!("{api}/actions/runs/{run_id}"),"head_sha":run.head_sha,"workflow_name":job.workflow_name,
        "head_branch":branch,"name":job.name,"url":format!("{api}/actions/jobs/{}",job.id),
        "html_url":format!("{}/{}/{}/actions?{query}",link_origin(),codeyard_webui::encode_component(owner),codeyard_webui::encode_component(repo)),
        "status":job.status,"conclusion":job.conclusion,"started_at":time(job.started_at.unwrap_or(job.created_at)),"completed_at":optional_time(job.completed_at),"created_at":time(job.created_at),
        "check_run_url":format!("{api}/check-runs/{}",job.check_run_id),"labels":job.labels.0,"runner_id":null,"runner_name":if run.engine.as_deref()==Some("codeyard-actions"){Some("codeyard-actions")}else{None},"runner_group_id":null,"runner_group_name":null,
        "steps":steps.iter().map(|step|json!({"number":step.number,"name":step.name,"status":step.status,"conclusion":step.conclusion,"started_at":time(step.started_at),"completed_at":optional_time(step.completed_at)})).collect::<Vec<_>>()})
}

pub fn action_artifact(
    object: &crate::backend::db::action_objects::ActionObject,
    run: Option<&codeyard_db_core::models::CiRun>,
    owner: &str,
    repo: &str,
) -> Value {
    let timestamp = |seconds| chrono::DateTime::from_timestamp(seconds, 0).map(time);
    let url = format!("{}/actions/artifacts/{}", repo_url(owner, repo), object.id);
    let workflow = run.map(|run| {
        let mut value = json!({"id":run.workflow_run_id.unwrap_or(run.id),"repository_id":run.repo_id,"head_branch":run.head_branch()});
        if let Some(sha) = &run.head_sha { value["head_sha"] = json!(sha); }
        if !run.is_fork { value["head_repository_id"]=json!(run.repo_id); }
        value
    });
    json!({"id":object.id,"node_id":node_id::encode("ART",object.id),"name":object.name,"size_in_bytes":object.size,"url":url,"archive_download_url":format!("{url}/zip"),"expired":object.expires_at<=chrono::Utc::now().timestamp(),"created_at":timestamp(object.created_at),"updated_at":timestamp(object.created_at),"expires_at":timestamp(object.expires_at),"digest":null,"workflow_run":workflow})
}

/// Secret alert documents are shared with the REST API; only authorized callers open the value.
pub fn secret_scanning_alert(
    alert: &crate::backend::db::secret_scanning::Alert,
    secret: &str,
    owner: &str,
    repo: &str,
    resolved_by: Option<&User>,
    bypass: Option<&crate::backend::db::secret_scanning::Bypass>,
    bypassed_by: Option<&User>,
) -> Value {
    let base = crate::backend::api::github::repo_url(owner, repo);
    let url = format!("{base}/secret-scanning/alerts/{}", alert.number);
    json!({
        "number": alert.number, "created_at": alert.created_at, "updated_at": alert.updated_at,
        "url": url, "html_url": format!("{}/{}/{}/security/secret-scanning/{}", crate::backend::api::http::config::link_origin(), codeyard_webui::encode_component(owner), codeyard_webui::encode_component(repo), alert.number),
        "locations_url": format!("{url}/locations"), "state": alert.state,
        "resolution": alert.resolution, "resolution_comment": alert.resolution_comment,
        "resolved_at": alert.resolved_at, "resolved_by": resolved_by.map(user),
        "secret_type": alert.secret_type,
        "provider_slug": crate::backend::repositories::secret_scanning::provider(&alert.secret_type), "secret_type_display_name": alert.display_name,
        "secret": secret, "validity": alert.validity.as_deref().unwrap_or("unknown"), "push_protection_bypassed": bypass.is_some(),
        "push_protection_bypassed_by": bypassed_by.map(user), "push_protection_bypassed_at": bypass.and_then(|bypass|bypass.used_at),
        "publicly_leaked": null, "multi_repo": null, "is_base64_encoded": false
    })
}

pub fn secret_scanning_location(
    location: &crate::backend::db::secret_scanning::Location,
    owner: &str,
    repo: &str,
) -> Value {
    let base = crate::backend::api::github::repo_url(owner, repo);
    json!({"type": "commit", "details": {
        "path": location.path, "start_line": location.start_line, "end_line": location.end_line,
        "start_column": location.start_column, "end_column": location.end_column,
        "blob_sha": location.blob_sha, "blob_url": format!("{base}/git/blobs/{}", location.blob_sha),
        "commit_sha": location.commit_sha, "commit_url": format!("{base}/commits/{}", location.commit_sha)
    }})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_branch_names_preserve_literal_ref_prefixes() {
        use codeyard_db_core::models::ci::workflow_branch;
        assert_eq!(workflow_branch("main"), "main");
        assert_eq!(workflow_branch("refs/heads/main"), "main");
        assert_eq!(workflow_branch("refs/tags/v1"), "v1");
        assert_eq!(workflow_branch("refs/heads/refs/tags/v1"), "refs/tags/v1");
        assert_eq!(
            workflow_branch("refs/heads/refs/heads/main"),
            "refs/heads/main"
        );
    }

    #[tokio::test]
    async fn rewinds_keep_the_head_and_large_messages_are_bounded() {
        tokio::task::spawn_blocking(|| {
            let root = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init_bare(root.path()).unwrap();
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Alice", "a@example.test").unwrap();
            let first = repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    &"é".repeat(10_000),
                    &tree,
                    &[],
                )
                .unwrap();
            let parent = repo.find_commit(first).unwrap();
            let second = repo
                .commit(None, &signature, &signature, "second", &tree, &[&parent])
                .unwrap();
            let rewound = pushed_commits(
                root.path(),
                Some(&second.to_string()),
                &first.to_string(),
                None,
            )
            .unwrap();
            assert!(rewound.commits.is_empty());
            let head = rewound.head.unwrap();
            assert_eq!(head.id, first.to_string());
            assert!(!head.distinct);
            assert_eq!(head.message.len(), 4096);
            assert!(head.message.is_char_boundary(head.message.len()));
            let forward = pushed_commits(
                root.path(),
                Some(&first.to_string()),
                &second.to_string(),
                None,
            )
            .unwrap();
            assert_eq!(forward.commits.len(), 1);
            assert!(forward.commits[0].distinct);
            assert_eq!(forward.head.unwrap().id, second.to_string());
            // A new ref at a commit another ref already held lists nothing new.
            let branch = pushed_commits(
                root.path(),
                None,
                &second.to_string(),
                Some(&[second.to_string()]),
            )
            .unwrap();
            assert!(branch.commits.is_empty());
            assert!(!branch.head.unwrap().distinct);
            assert_eq!(bounded_text("aé", 2), "a");
        })
        .await
        .unwrap();
    }

    #[test]
    fn push_marks_created_and_deleted_from_zero_ids() {
        const CHILD: &str = "CODEYARD_CLONE_URL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("push_marks_created_and_deleted_from_zero_ids")
                .env(CHILD, "1")
                .env("PUBLIC_URL", "https://web.example")
                .env("PUBLIC_HTTP_CLONE_BASE", "https://git.example/")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let repo = Repository {
            actions_engine: None,
            id: 1,
            owner_kind: "user".into(),
            owner_user_id: Some(7),
            owner_org_id: None,
            name: "app".into(),
            description: None,
            website: None,
            is_private: false,
            is_internal: false,
            is_gist: false,
            is_archived: false,
            is_fork: false,
            is_template: false,
            forked_from_id: None,
            default_branch: "main".into(),
            allow_merge_commit: true,
            allow_squash_merge: true,
            allow_rebase_merge: true,
            disk_path: String::new(),
            storage_node_id: None,
            size_bytes: 12_345,
            stars_count: 2,
            forks_count: 3,
            watchers_count: 4,
            open_issues_count: 5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let pusher = Person {
            login: "ann".into(),
            id: 2,
            site_admin: false,
        };
        let zero = "0".repeat(40);
        let value = push(
            "refs/heads/new",
            &zero,
            &"a".repeat(40),
            &PushedCommits::default(),
            &repo,
            "ann",
            &pusher,
        );
        assert_eq!(value["repository"]["size"], 12);
        assert_eq!(value["repository"]["stargazers_count"], 2);
        assert_eq!(value["repository"]["forks_count"], 0);
        // GitHub's historical watchers aliases mean stars; subscriptions are separate.
        assert_eq!(value["repository"]["watchers_count"], 2);
        assert_eq!(value["repository"]["watchers"], 2);
        assert_eq!(value["repository"]["subscribers_count"], 4);
        // A repository's creation time cannot stand in for an unknown push receipt.
        assert_eq!(value["repository"]["pushed_at"], Value::Null);
        assert_eq!(value["repository"]["open_issues_count"], 5);
        assert_eq!(value["created"], json!(true));
        assert_eq!(value["deleted"], json!(false));
        assert_eq!(value["repository"]["full_name"], json!("ann/app"));
        assert_eq!(
            value["repository"]["html_url"],
            "https://web.example/ann/app"
        );
        assert_eq!(
            value["repository"]["clone_url"],
            "https://git.example/ann/app.git"
        );
        assert_eq!(value["head_commit"], Value::Null);
    }
}
