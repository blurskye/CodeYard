//! Read-only discussion pages share the repository chrome and safe Markdown renderer.
use super::chrome::{chrome, RepoChrome};
use crate::{components::Markdown, navigation::NavLink};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Discussion {
    pub number: i32,
    pub title: String,
    pub body: String,
    pub author: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    pub id: i64,
    pub body: String,
    pub author: String,
    pub reply_to: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Index {
        discussions: Vec<Discussion>,
        page: usize,
        more: bool,
    },
    Detail {
        discussion: Discussion,
        comments: Vec<Comment>,
        thread: Option<i64>,
        page: usize,
        more: bool,
    },
}

pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    let base = format!("{}/discussions", repo.base());
    let body = match page {
        Page::Index {
            discussions,
            page,
            more,
        } => rsx! {
            section { class: "collaboration",
                h1 { "Discussions" }
                if discussions.is_empty() { p { "No discussions." } }
                for discussion in discussions {
                    article {
                        h2 { NavLink { href: format!("{base}/{}", discussion.number), "{discussion.title} #{discussion.number}" } }
                        p { "{discussion.author}" }
                    }
                }
                {pages(&base, *page, *more)}
            }
        },
        Page::Detail {
            discussion,
            comments,
            thread,
            page,
            more,
        } => {
            let target = format!("{base}/{}", discussion.number);
            let window = thread.map_or(target.clone(), |id| format!("{target}?thread={id}"));
            rsx! {
                section { class: "collaboration",
                    NavLink { href: base.clone(), "Discussions" }
                    h1 { "{discussion.title} #{discussion.number}" }
                    p { "{discussion.author}" }
                    Markdown { content: discussion.body.clone() }
                    for comment in comments {
                        article { id: format!("discussioncomment-{}", comment.id), class: "collaboration-comment",
                            p {
                                "{comment.author} · "
                                NavLink { href: format!("{target}?comment={0}#discussioncomment-{0}", comment.id), "Permalink" }
                            }
                            if let Some(parent) = comment.reply_to {
                                p { NavLink { href: format!("{target}?thread={parent}#discussioncomment-{parent}"), "View thread" } }
                            }
                            Markdown { content: comment.body.clone() }
                            if comment.reply_to.is_none() {
                                NavLink { href: format!("{target}?thread={}#discussioncomment-{}", comment.id, comment.id), "View replies" }
                            }
                        }
                    }
                    {pages(&window, *page, *more)}
                }
            }
        }
    };
    chrome(repo, body)
}
fn pages(base: &str, page: usize, more: bool) -> Element {
    let separator = if base.contains('?') { '&' } else { '?' };
    rsx! {
        nav { aria_label: "Discussion pages",
            if page > 1 { NavLink { href: format!("{base}{separator}page={}", page - 1), "Previous" } }
            if more { NavLink { href: format!("{base}{separator}page={}", page + 1), "Next" } }
        }
    }
}
