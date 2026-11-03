//! Administrator-only dependency inventory, advisory review and delegated update controls.
use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::{components::DraftInput, navigation::NavLink};
use dioxus::prelude::*;
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub intent: String,
    pub confirmation: String,
    pub consent: bool,
    pub alert: String,
    pub version: String,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub path: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    pub number: i32,
    pub state: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Alert {
    pub key: String,
    pub package: Package,
    pub advisory: String,
    pub summary: String,
    pub details: String,
    pub severity: String,
    pub url: String,
    pub fixed: Vec<String>,
    pub state: String,
    pub dismissal: Option<String>,
    pub manual: Option<String>,
    pub proposal: Option<Proposal>,
    pub pending: bool,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub alerts_enabled: bool,
    pub updates_enabled: bool,
    pub paused: bool,
    pub status: String,
    pub scan_sha: Option<String>,
    pub scan_date: Option<String>,
    pub scan_error: Option<String>,
    pub unsupported: usize,
    pub package_count: usize,
    pub packages: Vec<Package>,
    pub alerts: Vec<Alert>,
    pub page: usize,
    pub packages_page: usize,
    pub more: bool,
    pub packages_more: bool,
    pub filter: String,
    pub error: Option<String>,
    pub draft: Draft,
}
fn base(repo: &RepoChrome) -> String {
    format!("{}/security/dependencies", repo.base())
}
fn toggle(repo: &RepoChrome, page: &Page, feature: &str, enabled: bool) -> Element {
    let intent = format!("{feature}-{}", if enabled { "enable" } else { "disable" });
    let retry = page.draft.intent == intent;
    let confirmation = if retry {
        page.draft.confirmation.clone()
    } else {
        String::new()
    };
    let label = if enabled {
        format!(
            "Authorize {} and scan",
            if feature == "alerts" {
                "alerts"
            } else {
                "reviewed updates"
            }
        )
    } else {
        format!("Disable {feature}")
    };
    rsx! {form{method:"post",action:format!("{}/{feature}/{}",base(repo),if enabled{"enable"}else{"disable"}),class:"panel repo-form-body",
        {csrf(&repo.csrf_token)} {repository_id(repo.id)}
        label{class:"repo-field",span{"Type the repository name to confirm: {repo.name}"} DraftInput{name:"confirmation",required:true,initial_value:confirmation}}
        if enabled&&repo.is_private{label{input{r#type:"checkbox",name:"consent",required:true,checked:retry&&page.draft.consent} "I authorize sending private dependency names and versions to the npm registry for verified update proposals."}}
        button{r#type:"submit",class:"btn btn-secondary","{label}"}
    }}
}
pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    let root = base(repo);
    chrome(
        repo,
        rsx! {section{class:"security-page",
            header{class:"repo-page-heading",h2{"Dependency security"} p{"Confidential inventory and reviews are available only to current repository administrators."}}
            NavLink{href:format!("{}/security",repo.base()),"Back to security reports"}
            if let Some(error)=&page.error{p{class:"form-error",role:"alert","{error}"}}
            if !page.draft.reason.is_empty()&&(repo.is_archived||!page.alerts.iter().any(|a|a.key==page.draft.alert&&a.package.version==page.draft.version)){section{class:"panel",h3{"Unsent review"} pre{"{page.draft.reason}"}}}
            section{class:"panel repo-form-body",h3{"Status"}
                p{"Alerts: " if page.alerts_enabled{"enabled"}else{"disabled"} " · Reviewed updates: " if page.updates_enabled{"enabled"}else{"disabled"} if page.paused{" · paused"}}
                p{"{page.status}"}
                if let Some(error)=&page.scan_error{p{class:"form-error","{error}"}}
                if let Some(date)=&page.scan_date{p{"Last successful scan: {date}"}}
                if let Some(sha)=&page.scan_sha{p{"Pinned default-branch commit: " code{"{sha}"}}}else{p{"No completed dependency scan yet. An empty list is not a security guarantee."}}
                p{"The shared dependency inventory and Dependabot alerts supply findings. Verified proposals support root npm package-lock v1–v3 only; other sources and ecosystems need manual updates."}
                p{"Advisories come from the locally synchronized OSV catalogue. Updates query the official npm registry and download bounded archives. Enabling authorizes registry disclosures, including for private repositories."}
                p{"Background proposals use the enabling administrator’s live authority. Revocation, ownership or privacy changes pause work until reauthorized. Only verified nonzero-major stable direct patch updates with unchanged requirements are proposed; other alerts remain manual."}
                p{"No package manager or repository scripts are run by the scanner. Ordinary repository CI MAY execute on admitted update branches. Registry verification is not a code audit. Review every pull request; updates are never auto-merged."}
                if repo.is_archived{p{"Archived repository: controls are read-only."}}
            }
            if !repo.is_archived{section{class:"repo-settings-grid",
                {toggle(repo,page,"alerts",true)}
                if page.alerts_enabled{{toggle(repo,page,"alerts",false)}}
                if page.alerts_enabled{{toggle(repo,page,"updates",true)}}
                if page.updates_enabled{{toggle(repo,page,"updates",false)}}
            }}
            section{class:"panel repo-form-body",h3{"Advisory reviews"}
                nav{aria_label:"Review status",for filter in ["open","dismissed","resolved","all"]{NavLink{href:format!("{root}?state={filter}&packages_page={}",page.packages_page),"{filter} "}}}
                if page.alerts.is_empty(){p{"No matching reviews. Unsupported or unscanned dependencies may still be vulnerable."}}
                for alert in &page.alerts{article{key:"{alert.key}",class:"panel repo-form-body",
                    h4{"{alert.package.name} {alert.package.version} · {alert.advisory}"}
                    code{"{alert.package.path}"} p{"{alert.state} · {alert.severity}"} p{"{alert.summary}"}
                    details{summary{"Advisory excerpt"} pre{"{alert.details}"}}
                    a{href:alert.url.clone(),rel:"noreferrer noopener",target:"_blank","Full advisory at OSV"}
                    if !alert.fixed.is_empty(){p{"Stable patch candidates: " code{{alert.fixed.join(", ")}}}}
                    if let Some(reason)=&alert.dismissal{p{"Dismissed: {reason}"}}
                    if let Some(reason)=&alert.manual{p{"Manual review required: {reason}"}}
                    if alert.pending{p{"An update intent is recorded; no pull request receipt is available yet."}}
                    if let Some(proposal)=&alert.proposal{p{NavLink{href:format!("{}/pulls/{}",repo.base(),proposal.number),"Proposal #" "{proposal.number}"} " · {proposal.state}"}}
                    NavLink{href:format!("{}/security/dependabot",repo.base()),"Review in Dependabot alerts"}
                }}
                nav{class:"pagination",aria_label:"Advisory pages",
                    if page.page>1{NavLink{href:format!("{root}?state={}&page={}&packages_page={}",page.filter,page.page-1,page.packages_page),"Previous reviews"}}
                    if page.more{NavLink{href:format!("{root}?state={}&page={}&packages_page={}",page.filter,page.page+1,page.packages_page),"Next reviews"}}
                }
            }
            section{class:"panel repo-form-body",h3{"Scanned inventory ({page.package_count})"}
                for package in &page.packages{p{strong{"{package.name} {package.version}"} br{} code{"{package.path}"}}}
                nav{class:"pagination",aria_label:"Inventory pages",
                    if page.packages_page>1{NavLink{href:format!("{root}?state={}&page={}&packages_page={}",page.filter,page.page,page.packages_page-1),"Previous dependencies"}}
                    if page.packages_more{NavLink{href:format!("{root}?state={}&page={}&packages_page={}",page.filter,page.page,page.packages_page+1),"Next dependencies"}}
                }
            }
        }},
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (RepoChrome, Page) {
        let repo = RepoChrome {
            id: 7,
            owner: "alice".into(),
            name: "deps".into(),
            csrf_token: "csrf-fixture".into(),
            signed_in: true,
            can_admin: true,
            is_private: true,
            ..Default::default()
        };
        let page = Page {
            alerts_enabled: true,
            updates_enabled: true,
            paused: false,
            status: "complete".into(),
            scan_sha: Some("a".repeat(40)),
            scan_date: Some("2026-10-01T00:00:00Z".into()),
            scan_error: None,
            unsupported: 1,
            package_count: 1,
            packages: vec![Package {
                name: "fixture".into(),
                version: "1.2.0".into(),
                path: "node_modules/fixture".into(),
            }],
            alerts: vec![Alert {
                key: "a".repeat(64),
                package: Package {
                    name: "fixture".into(),
                    version: "1.2.0".into(),
                    path: "node_modules/fixture".into(),
                },
                advisory: "GHSA-test".into(),
                summary: "An advisory".into(),
                details: "</pre><img src=x onerror=alert(1)>".into(),
                severity: "HIGH".into(),
                url: "https://osv.dev/vulnerability/GHSA-test".into(),
                fixed: vec!["1.2.1".into()],
                state: "open".into(),
                dismissal: None,
                manual: Some("Manual review".into()),
                proposal: Some(Proposal {
                    number: 3,
                    state: "closed".into(),
                }),
                pending: false,
            }],
            page: 1,
            packages_page: 1,
            more: false,
            packages_more: false,
            filter: "open".into(),
            error: None,
            draft: Draft::default(),
        };
        (repo, page)
    }
    #[test]
    fn dependency_page_round_trips_and_renders_safe_real_controls() {
        let (repo, page) = fixture();
        let model = super::super::page::Page::Dependencies {
            repo: repo.clone(),
            page: Box::new(page.clone()),
        };
        let decoded: super::super::page::Page =
            serde_json::from_value(serde_json::to_value(model).unwrap()).unwrap();
        assert_eq!(decoded.repo_chrome().id, 7);
        let html = crate::tests::in_scope(|| dioxus_ssr::render_element(render(&repo, &page)));
        for text in [
            "OSV",
            "MAY execute",
            "never auto-merged",
            "repository_id",
            "csrf-fixture",
            "/alice/deps/pulls/3",
            "/alice/deps/security/dependencies/alerts/enable",
            "consent",
        ] {
            assert!(html.contains(text), "{text}");
        }
        assert!(!html.contains("<img src=x"));
        assert!(
            html.contains("&#60;img src=x onerror=alert(1)&#62;"),
            "{html}"
        );
    }
    #[test]
    fn archive_and_stale_reviews_keep_drafts_without_rebinding_actions() {
        let (mut repo, mut page) = fixture();
        page.draft = Draft {
            alert: "a".repeat(64),
            version: "0.9.0".into(),
            reason: "old-reviewed-version".into(),
            ..Default::default()
        };
        let html = crate::tests::in_scope(|| dioxus_ssr::render_element(render(&repo, &page)));
        assert!(html.contains("Unsent review"));
        assert_eq!(html.matches("old-reviewed-version").count(), 1);
        repo.is_archived = true;
        page.draft.version = "1.2.0".into();
        let html = crate::tests::in_scope(|| dioxus_ssr::render_element(render(&repo, &page)));
        assert!(html.contains("old-reviewed-version"));
        assert!(html.contains("controls are read-only"));
        assert!(!html.contains("action=\"/alice/deps/security/dependencies/"));
    }
}
