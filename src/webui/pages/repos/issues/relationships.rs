use super::{CommentPage, Repository};
use crate::webui::{navigation::NavLink, pages::repos::chrome::csrf};
use dioxus::prelude::*;
fn change_form(
    repo: &Repository,
    url: &str,
    kind: &str,
    id: i64,
    position: Option<(&str, i64)>,
    label: &str,
) -> Element {
    rsx! {form {method:"post",action:format!("{url}/relationships"),
        {csrf(&repo.csrf)}
        input {r#type:"hidden",name:"kind",value:kind}
        input {r#type:"hidden",name:"action",value:if position.is_some(){"move"}else{"remove"}}
        input {r#type:"hidden",name:"issue_id",value:id.to_string()}
        if let Some((name,id))=position {input {r#type:"hidden",name,value:id.to_string()}}
        button {r#type:"submit",class:"btn btn-secondary btn-sm",aria_label:label,"{label}"}
    }}
}
pub(super) fn panel(repo: &Repository, url: &str, comments: &CommentPage) -> Element {
    let writer = repo.can_write && !repo.chrome.is_archived;
    let groups = [
        ("parent", "Parent issue"),
        ("children", "Sub-issues"),
        ("blocked_by", "Blocked by"),
        ("blocking", "Blocking"),
        ("related", "Related issues"),
    ];
    rsx! {section {class:"collaboration-sidebar-section",aria_label:"Issue relationships",
        h3 {"Relationships"}
        for (kind,title) in groups {
            {let items=comments.relationships.iter().filter(|item|item.kind==kind).collect::<Vec<_>>();rsx!{
                if !items.is_empty(){
                    h4 {"{title}"}
                    ul {class:"collaboration-picker-list",
                        for (index,item) in items.iter().enumerate(){li {
                            NavLink {class:"collaboration-title-link",href:item.href.clone(),"{item.reference}: {item.title}"}
                            span {class:"collaboration-muted"," ({item.state})"}
                            if writer && matches!(kind,"children"|"blocked_by"|"related") {
                                div {class:"collaboration-actions",
                                    {change_form(repo,url,kind,item.id,None,"Remove")}
                                    if kind=="children" {
                                        if let Some(previous)=index.checked_sub(1).and_then(|at|items.get(at)){ {change_form(repo,url,kind,item.id,Some(("before_id",previous.id)),"Move earlier")} }
                                        if let Some(next)=items.get(index+1){ {change_form(repo,url,kind,item.id,Some(("after_id",next.id)),"Move later")} }
                                    }
                                }
                            }
                        }}
                    }
                }
            }}
        }
        if comments.relationships.is_empty(){p {class:"collaboration-muted","No linked issues."}}
        if writer {form {method:"post",action:format!("{url}/relationships"),class:"collaboration-form-fields",
            {csrf(&repo.csrf)}
            input {r#type:"hidden",name:"action",value:"add"}
            label {r#for:"relationship-kind","Relationship"}
            select {id:"relationship-kind",name:"kind",class:"collaboration-input",
                option {value:"children","Sub-issue"} option {value:"blocked_by","Blocked by"} option {value:"related","Related issue"}
            }
            label {r#for:"relationship-reference","Issue reference"}
            input {id:"relationship-reference",name:"reference",placeholder:"owner/repository#number",required:true,class:"collaboration-input"}
            label {input {r#type:"checkbox",name:"replace_parent",value:"true"} " Replace current parent"}
            button {r#type:"submit",class:"btn btn-secondary btn-sm","Link issue"}
        }}
    }}
}
