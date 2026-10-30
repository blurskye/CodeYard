use super::*;
use crate::webui::{
    components::{
        self, CopyButton, DraftCheckbox, DraftInput, DraftTextarea, LinkBase, Markdown,
        RepoMarkdown,
    },
    navigation::NavLink,
    pages::{discovery::page_links, fields::csrf},
};
use dioxus::prelude::*;
pub fn render(page: Page) -> Element {
    match page {
        Page::List(page) => listing(&page),
        Page::Detail(page) => detail(&page),
        Page::Editor(page) => editor(&page),
    }
}
fn badge(public: bool) -> Element {
    rsx! {span {class:"gist-badge",if public {"Public"} else {"Secret"}}}
}
fn card(card: &Card) -> Element {
    let first = card
        .filenames
        .first()
        .map(String::as_str)
        .unwrap_or(&card.id);
    rsx! {article {class:"gist-card",
        h2 {NavLink {href:crate::webui::profile_path(&card.owner),"{card.owner}"} " / " NavLink {href:format!("/gists/{}",card.id),"{first}"}}
        p {class:"gist-description","{card.description}"}
        div {class:"gist-meta",{badge(card.public)} if card.files_unavailable {span {"Files unavailable"}} else {span {"{card.file_count} files"}} span {"{card.comments} comments"} span {"Updated " components::LiveTime {value:card.updated_at.clone()}}}
    }}
}
fn listing(page: &Listing) -> Element {
    rsx! {main {class:"gists-page",
        header {class:"gist-heading",h1 {if let Some(owner)=&page.owner {"{owner}’s gists"} else {"Gists"}} if page.signed_in {NavLink {href:"/gists/new",class:"btn btn-primary","New gist"}}}
        p {class:"text-muted-foreground","Share small files with real Git history."}
        nav {class:"gist-toolbar","aria-label":"Gist lists",NavLink {href:"/gists?tab=public",class:if page.tab=="public" {"btn gist-selected"} else {"btn"},"Public gists"} if page.signed_in {NavLink {href:"/gists?tab=mine",class:if page.tab=="mine" {"btn gist-selected"} else {"btn"},"Your gists"} NavLink {href:"/gists?tab=starred",class:if page.tab=="starred" {"btn gist-selected"} else {"btn"},"Starred"}}}
        if page.cards.is_empty() {p {class:"gist-empty","No gists in this list."}} else {for entry in &page.cards {{card(entry)}}}
        {page_links(page.previous.clone(),page.next.clone(),"Gists")}
    }}
}
fn editor(page: &Editor) -> Element {
    let action = page.id.as_ref().map_or_else(
        || "/gists/new".to_string(),
        |id| format!("/gists/{id}/edit"),
    );
    let cancel = page
        .id
        .as_ref()
        .map_or_else(|| "/gists".to_string(), |id| format!("/gists/{id}"));
    rsx! {main {class:"gists-page",
        header {class:"gist-heading",h1 {if page.id.is_some() {"Edit gist"} else {"Create a gist"}} NavLink {href:cancel.clone(),class:"btn","Cancel"}}
        if let Some(message)=&page.message {p {class:"gist-notice",role:"alert","{message}"}}
        p {class:"text-muted-foreground","Keep 1–100 files, at most 10 MiB in total. This browser editor accepts 1 MiB of text; use Git for larger or binary files."}
        form {method:"post",action,
            {csrf(&page.csrf_token)} input {r#type:"hidden",name:"expected",value:page.expected.clone()} input {r#type:"hidden",name:"file_count",value:page.files.len().to_string()}
            label {class:"gist-field",span {"Description"} DraftTextarea {name:"description",initial_value:page.description.clone(),rows:3,maxlength:Some(65535),placeholder:"What is this gist about?"}}
            for (index,file) in page.files.iter().enumerate() {section {key:"file-{index}",class:"gist-editor-file",
                if let Some(original)=&file.original {input {r#type:"hidden",name:"original_{index}",value:original.clone()}}
                label {class:"gist-field",span {"Filename"} DraftInput {name:format!("name_{index}"),initial_value:file.filename.clone(),maxlength:Some(255),placeholder:"filename.txt",aria_label:Some(format!("Filename {}",index+1))}}
                if let Some(content)=&file.content {label {class:"gist-field",span {"Content"} DraftTextarea {name:format!("content_{index}"),initial_value:content.clone(),rows:12,maxlength:Some(1048576),class:"gist-editor-content"}}} else {p {class:"gist-notice","Binary or large file: its contents will be kept unchanged. Use Git to edit its contents."}}
                label {class:"gist-checkbox",DraftCheckbox {name:format!("delete_{index}"),initial_checked:file.delete,submission_value:"yes"} "Delete this file"}
            }}
            if page.id.is_none() {label {class:"gist-checkbox",DraftCheckbox {name:"public",initial_checked:page.public,submission_value:"yes"} "Make this gist public"}} else {div {class:"gist-meta",{badge(page.public)} span {"Visibility cannot be changed after creation."}}}
            p {class:"text-muted-foreground","Secret gists are not listed publicly. Anyone with their link can still read or clone them."}
            div {class:"gist-toolbar",button {r#type:"submit",class:"btn btn-primary",name:"intent",value:"save",if page.id.is_some() {"Save gist"} else {"Create gist"}} if page.files.len()<200 {button {r#type:"submit",class:"btn",name:"intent",value:"add-file",formnovalidate:true,"Add another file"}} NavLink {href:cancel,"Cancel"}}
        }
    }}
}
fn detail(page: &Detail) -> Element {
    let base = format!("/gists/{}", page.id);
    let raw = format!("{base}/raw/{}", page.revision);
    rsx! {main {class:"gists-page",
        header {class:"gist-heading",div {NavLink {href:crate::webui::profile_path(&page.owner),"{page.owner}"} h1 {"Gist " span {class:"gist-id","{page.id}"}}} {badge(page.public)}}
        p {class:"gist-description","{page.description}"}
        if !page.public {p {class:"text-muted-foreground","Secret gist — anyone with this link can read or clone it."}}
        if let Some(parent)=&page.parent {p {"Forked from " NavLink {href:format!("/gists/{parent}"),"{parent}"}}}
        if let Some(message)=&page.message {p {class:"gist-notice",role:"alert","{message}"}}
        nav {class:"gist-toolbar","aria-label":"Gist actions",NavLink {href:"/gists","Gists"} if page.own {NavLink {href:format!("{base}/edit"),class:"btn","Edit"}} if page.signed_in {form {method:"post",action:format!("{base}/star"),{csrf(&page.csrf_token)} input {r#type:"hidden",name:"add",value:if page.starred {"no"} else {"yes"}} button {r#type:"submit",class:"btn",if page.starred {"Unstar"} else {"Star"}}} if !page.own {form {method:"post",action:format!("{base}/fork"),{csrf(&page.csrf_token)} button {r#type:"submit",class:"btn","Fork"}}}} else {NavLink {href:format!("/enter?redirect={}",crate::webui::encode_component(&base)),"Sign in to star, fork or comment"}}}
        div {class:"gist-clone",code {"git clone {page.git_url}"} CopyButton {text:format!("git clone {}",page.git_url),label:"Copy clone command"}}
        p {class:"gist-meta","Revision " code {class:"gist-id","{page.revision}"} if page.historical {NavLink {href:base.clone(),"View latest revision"}}}
        for file in &page.files {section {class:"gist-file",key:"{file.filename}",header {class:"gist-file-heading",h2 {"{file.filename}"} div {class:"gist-meta",span {"{components::format_bytes(file.size as i64)}"} a {href:file.raw_url.clone(),class:"btn btn-sm","Raw"}}}
            if let Some(content)=&file.content {if !file.truncated && matches!(std::path::Path::new(&file.filename).extension().and_then(|extension|extension.to_str()),Some("md"|"markdown")) {div {class:"gist-markdown",RepoMarkdown {content:content.clone(),base:LinkBase {blob:raw.clone(),tree:raw.clone(),raw:raw.clone(),directory:String::new()}}}} else {pre {class:"gist-code",code {dangerous_inner_html:components::code::themed_highlight(content,components::code::language_of_path(&file.filename))}}}
                if file.truncated {p {class:"gist-file-notice","Preview truncated. Raw contains the complete file."}}
            } else if file.image {img {class:"gist-image",src:file.raw_url.clone(),alt:file.filename.clone(),loading:"lazy"}} else {p {class:"gist-file-notice","Binary file. " a {href:file.raw_url.clone(),"Download raw file"}}}
        }}
        section {class:"gist-section",h2 {"Revision history"} ul {class:"gist-history",for revision in &page.revisions {li {NavLink {href:format!("{base}?revision={}",revision.sha),"{&revision.sha[..7]}"} span {if let Some(author)=&revision.author {"{author}"} else {"Unknown author"}} components::LiveTime {value:revision.committed_at.clone()} span {"+{revision.additions} −{revision.deletions}"}}}} {page_links(page.history_previous.clone(),page.history_next.clone(),"Revisions")}}
        section {class:"gist-section",h2 {"Forks"} if page.forks.is_empty() {p {class:"text-muted-foreground","No forks yet."}} else {for fork in &page.forks {{card(fork)}}} {page_links(page.forks_previous.clone(),page.forks_next.clone(),"Forks")}}
        section {class:"gist-section",h2 {"Comments"} if page.comments.is_empty() {p {class:"text-muted-foreground","No comments yet."}}
            for comment in &page.comments {article {class:"gist-comment",key:"comment-{comment.id}",id:"comment-{comment.id}",header {class:"gist-meta",if let Some(author)=&comment.author {NavLink {href:crate::webui::profile_path(author),"{author}"}} else {span {"Deleted account"}} components::LiveTime {value:comment.created_at.clone()}}
                Markdown {content:comment.body.clone()}
                if comment.can_edit {details {class:"gist-comment-edit",summary {"Edit comment"} form {method:"post",action:format!("{base}/comments/{}/edit?comments_page={}",comment.id,page.comments_page),{csrf(&page.csrf_token)} label {class:"gist-field",span {"Comment"} DraftTextarea {name:"body",initial_value:comment.edit_body.as_ref().unwrap_or(&comment.body).clone(),required:true,maxlength:Some(65535),rows:5}} button {r#type:"submit",class:"btn","Save comment"}}}}
                if comment.can_delete {details {class:"gist-comment-edit",summary {"Delete comment"} form {method:"post",action:format!("{base}/comments/{}/delete?comments_page={}",comment.id,page.comments_page),{csrf(&page.csrf_token)} p {"Delete this comment permanently?"} button {r#type:"submit",class:"btn btn-danger","Confirm delete comment"}}}}
            }}
            {page_links(page.comments_previous.clone(),page.comments_next.clone(),"Comments")}
            if page.signed_in {form {method:"post",action:format!("{base}/comments?comments_page={}",page.comments_page),{csrf(&page.csrf_token)} label {class:"gist-field",span {"Write a comment (Markdown supported)"} DraftTextarea {name:"body",initial_value:page.comment_body.clone(),required:true,maxlength:Some(65535),rows:5}} button {r#type:"submit",class:"btn btn-primary","Comment"}}}
        }
        if page.own {details {class:"gist-delete",summary {class:"btn btn-danger","Delete gist"} form {method:"post",action:format!("{base}/delete"),{csrf(&page.csrf_token)} p {"Permanently delete this gist, its comments and its stars? Existing forks are kept."} button {r#type:"submit",class:"btn btn-danger","Confirm delete gist"}}}}
    }}
}
