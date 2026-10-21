use super::*;

/// Markdown for text that names no repository, so nothing links.
fn parse_markdown(text: &str) -> String {
    super::parse_markdown(text, &References::default())
}

fn parse_document(text: &str) -> String {
    super::parse_document(text, &References::default())
}

fn bounded_markdown(text: &str, document: bool) -> Option<String> {
    super::bounded_markdown(text, document, &References::default())
}

#[test]
fn repository_urls_decode_entities_before_resolving_fragments() {
    let base = LinkBase {
        blob: "/u/r/blob/main".into(),
        tree: "/u/r/tree/main".into(),
        raw: "/u/r/raw/main".into(),
        directory: String::new(),
    };
    let html = super::document_html(
        "# Usage\n\n[guide](guide.md?name=O'Reilly&x=1#usage)",
        &References::default(),
        &base,
    );
    assert!(
        html.contains(
            "href=\"/u/r/blob/main/guide.md?name=O&#39;Reilly&amp;x=1#user-content-usage\""
        ),
        "{html}"
    );
    assert!(!html.contains("&amp;#39;"));
    let outline = super::outline_of_html(&html);
    assert_eq!(outline[0].id, "user-content-usage");
}

#[test]
fn outline_lists_headings_with_the_ids_their_sections_have() {
    let source =
        "# Title\n\ntext\n\n## Setup & install\n\n### `cargo` steps\n\n## Setup & install\n";
    let html = parse_document(source);
    let outline = super::outline_of_html(&html);
    let levels: Vec<usize> = outline.iter().map(|entry| entry.level).collect();
    assert_eq!(levels, vec![1, 2, 3, 2]);
    assert_eq!(outline[1].text, "Setup & install");
    assert_eq!(outline[2].text, "cargo steps");
    assert_ne!(outline[1].id, outline[3].id);
    for entry in &outline {
        assert!(html.contains(&format!("id=\"{}\"", entry.id)));
    }
    assert!(super::outline_of_html(&parse_document("No headings here.")).is_empty());
}

fn base() -> LinkBase {
    LinkBase {
        blob: "/alice/demo/blob/main".into(),
        tree: "/alice/demo/tree/main".into(),
        raw: "/alice/demo/raw/main".into(),
        directory: "docs".into(),
    }
}

#[test]
fn rewriting_many_links_stays_linear() {
    let html = "<a href=\"x.md\">x</a>\n".repeat(70_000);
    let started = std::time::Instant::now();
    let rewritten = base().rewrite(&html).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(
        rewritten.matches("/alice/demo/blob/main/docs/x.md").count(),
        70_000
    );
}

#[test]
fn generated_markdown_bytes_are_bounded() {
    // Lines of a comment are joined with `<br>`, so a line costs less than it did as a
    // paragraph; 90,000 of them still pass the budget.
    for source in ["[x](x)".repeat(90_000), "[x](x)\n".repeat(90_000)] {
        assert!(bounded_markdown(&source, false).is_none());
        let html = parse_markdown(&source);
        assert!(html.contains("showing a plain-text preview"));
        assert!(html.len() < MAX_PLAIN_TEXT_BYTES * 6 + 512);
        assert!(!html.contains("<a "));
    }
}

#[test]
fn expanded_repository_urls_are_charged_to_the_output_budget() {
    let mut base = base();
    base.directory = vec!["d".repeat(255); 12].join("/");
    for source in ["[x](x)\n", "![x](x)\n"] {
        let html = bounded_markdown(&source.repeat(4_000), true).unwrap();
        assert!(html.len() < MAX_MARKDOWN_BYTES);
        assert!(base.rewrite(&html).is_none());
    }
}

#[test]
fn over_long_bases_leave_links_images_and_srcsets_as_written() {
    use crate::webui::pages::repos::ref_path::link_base;
    let long = link_base("/alice/demo", &"b".repeat(5_000), "docs");
    let candidates = ["img/a.png 1x"; 16].join(", ");
    let source = format!(
            "[guide](guide.md) ![pic](img/a.png)\n\n<picture><source srcset=\"{candidates}\"><img src=\"img/a.png\"></picture>"
        );
    let html = parse_document(&source);
    let rewritten = long.rewrite(&html).unwrap();
    assert_eq!(rewritten, html);
    assert!(rewritten.len() < 4 * 1024);
    let normal = link_base("/alice/demo", "main", "docs");
    assert!(normal
        .rewrite(&html)
        .unwrap()
        .contains("href=\"/alice/demo/blob/refs/heads/main/docs/guide.md\""));
}

#[test]
fn rewrite_budget_includes_unchanged_html_and_the_last_url() {
    let padding = "x".repeat(MAX_MARKDOWN_BYTES);
    assert_eq!(base().rewrite(&padding).unwrap().len(), MAX_MARKDOWN_BYTES);
    assert!(base().rewrite(&format!("{padding}x")).is_none());
    let link = "<a href=\"x\">x</a>";
    let expanded = base().rewrite(link).unwrap();
    let padding = &padding[..MAX_MARKDOWN_BYTES - expanded.len()];
    assert_eq!(
        base().rewrite(&format!("{padding}{link}")).unwrap().len(),
        MAX_MARKDOWN_BYTES
    );
    assert!(base().rewrite(&format!("x{padding}{link}")).is_none());
}

#[test]
fn plain_text_preview_escapes_html_and_preserves_utf8_boundaries() {
    let start = "<script>";
    let source = format!(
        "{start}{}雪hidden-tail",
        "x".repeat(MAX_PLAIN_TEXT_BYTES - start.len() - 1)
    );
    let html = plain_text_preview(&source);
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains('雪'));
    assert!(!html.contains("hidden-tail"));
    assert!(html.ends_with("</code></pre>"));
    assert!(html.len() < MAX_PLAIN_TEXT_BYTES * 6 + 512);
}

#[test]
fn task_list_items_render_read_only_checkboxes() {
    let html = parse_markdown("- [ ] open\n- [x] done");
    assert!(html.contains("<input type=\"checkbox\" disabled class=\"mr-2\">open"));
    assert!(html.contains("<input type=\"checkbox\" disabled checked class=\"mr-2\">done"));
    assert!(!html.contains("[ ]") && !html.contains("[x]"));
}

#[test]
fn relative_links_and_images_resolve_against_the_files_directory() {
    let html = base()
        .rewrite(&parse_markdown(
            "[guide](guide.md#top) [up](../README.md) [src](../src/) ![pic](img/a.png)",
        ))
        .unwrap();
    let guide = "/alice/demo/blob/main/docs/guide.md#user-content-top";
    assert!(html.contains(&format!("href=\"{guide}\" class=")));
    assert!(!html.contains(&format!("{guide}\" target")));
    assert!(html.contains("href=\"/alice/demo/blob/main/README.md\""));
    assert!(html.contains("href=\"/alice/demo/tree/main/src\""));
    assert!(html.contains("src=\"/alice/demo/raw/main/docs/img/a.png\""));
}

#[test]
fn a_link_to_the_repository_root_opens_the_tree() {
    let html = base().rewrite(&parse_markdown("[root](..)")).unwrap();
    assert!(html.contains("href=\"/alice/demo/tree/main\""), "{html}");
}

#[test]
fn fragments_into_markdown_files_match_their_heading_ids() {
    let html = base()
        .rewrite(&parse_markdown(
            "[details](guide.md#details) [code](main.rs#L10)",
        ))
        .unwrap();
    let guide = "/alice/demo/blob/main/docs/guide.md#user-content-details";
    assert!(html.contains(&format!("href=\"{guide}\"")));
    assert!(html.contains("href=\"/alice/demo/blob/main/docs/main.rs#L10\""));
}

#[test]
fn fragments_decode_percent_escapes_and_cover_every_markdown_extension() {
    let html = base()
            .rewrite(&parse_markdown(
                "[a](#caf%C3%A9) [b](guide.markdown#setup) [c](guide.mdx#setup) [d](guide.md?view=code#L10)",
            ))
            .unwrap();
    assert!(html.contains("href=\"#user-content-café\""), "{html}");
    assert!(
        html.contains("guide.markdown#user-content-setup\""),
        "{html}"
    );
    assert!(html.contains("guide.mdx#user-content-setup\""), "{html}");
    assert!(html.contains("guide.md?view=code#L10\""), "{html}");
}

#[test]
fn discussion_fragments_keep_comment_anchors_and_prefixed_ids() {
    let html = parse_markdown(
            "[comment](#comment-23) [heading](#user-content-comment-23) [top](#)\n## Comment 23\n[heading](#comment-policy)",
        );
    assert!(html.contains("href=\"#comment-23\""));
    assert!(html.contains("id=\"user-content-comment-23\""));
    assert!(html.contains("href=\"#user-content-comment-23\""));
    assert!(html.contains("href=\"#\""));
    assert!(html.contains("href=\"#user-content-comment-policy\""));
    assert!(!html.contains("user-content-user-content-"));
}

#[test]
fn repository_comment_fragments_name_headings() {
    let html = base().rewrite(&parse_markdown(
            "[comment](#comment-23) [top](#)\n## Comment 23\n[remote](guide.md#comment-23) [prefixed](guide.md#user-content-details)",
        )).unwrap();
    assert!(html.contains("href=\"#user-content-comment-23\""));
    assert!(html.contains("href=\"#\""));
    assert!(html.contains("href=\"/alice/demo/blob/main/docs/guide.md#user-content-comment-23\""));
    assert!(html.contains("href=\"/alice/demo/blob/main/docs/guide.md#user-content-details\""));
    assert!(!html.contains("user-content-user-content-"));
}

#[test]
fn absolute_anchor_and_scheme_links_are_left_alone() {
    let parsed = parse_markdown(
        "[a](https://example.test/x) [b](#section) [c](/explore) [d](mailto:a@example.test)",
    );
    let html = base().rewrite(&parsed).unwrap();
    assert!(html.contains("href=\"https://example.test/x\" target=\"_blank\""));
    assert!(html.contains("href=\"#user-content-section\" class="));
    assert!(html.contains("href=\"/explore\""));
    assert!(html.contains("href=\"mailto:a@example.test\""));
}

#[test]
fn raw_html_links_and_images_are_rewritten_like_markdown() {
    let html = base()
        .rewrite(&parse_markdown(
            "<a href=\"x.md\">x</a> <img src=\"img/a.png\">",
        ))
        .unwrap();
    assert!(html.contains(
        "<a href=\"/alice/demo/blob/main/docs/x.md\" class=\"text-primary hover:underline\">x</a>"
    ));
    assert!(html.contains("src=\"/alice/demo/raw/main/docs/img/a.png\""));
}

#[test]
fn a_reference_inside_a_raw_anchor_stays_text() {
    let html = super::parse_markdown("Text <a href=\"/bob\">#5</a>", &page_references());
    assert!(!html.contains("markdown-reference"), "{html}");
    assert!(html.contains(">#5</a>"), "{html}");
}

#[test]
fn a_bare_url_inside_a_raw_anchor_stays_text() {
    let html = super::parse_markdown(
        "Text <a href=\"x\">http://y.example</a>",
        &page_references(),
    );
    assert_eq!(html.matches("<a ").count(), 1, "{html}");
    assert!(html.contains(">http://y.example</a>"), "{html}");
}

#[test]
fn a_sha_inside_raw_code_stays_text() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let html = super::parse_markdown(&format!("Text <code>{sha}</code>"), &page_references());
    assert!(!html.contains("markdown-commit"), "{html}");
    assert!(html.contains(&format!("<code>{sha}</code>")), "{html}");
}

#[test]
fn a_reference_inside_raw_pre_stays_text() {
    let html = super::parse_markdown("Text <pre>@bob</pre>", &page_references());
    assert!(!html.contains("markdown-reference"), "{html}");
    assert!(html.contains("<pre>@bob</pre>"), "{html}");
}

#[test]
fn references_after_a_raw_closing_tag_link_again() {
    let html = super::parse_markdown(
        "<code>x</code> #5 and <a href=\"/bob\">y</a> @bob",
        &page_references(),
    );
    assert!(
        html.contains("<a href=\"/alice/demo/issues/5\" class=\"markdown-reference\">#5</a>"),
        "{html}"
    );
    assert!(
        html.contains("<a href=\"/bob\" class=\"markdown-reference\">@bob</a>"),
        "{html}"
    );
}

#[test]
fn a_code_block_that_spends_the_user_budget_does_not_hide_a_prose_reference() {
    let names: Vec<String> = (0..50).map(|n| format!("@user{n}")).collect();
    let source = format!("```\n{}\n```\n\nThanks @bob", names.join(" "));
    let found = references::candidates(&[source.as_str()], false, ("alice", "demo"));
    assert_eq!(found.users, vec!["bob".to_owned()]);
    let resolved = References {
        users: found.users,
        ..page_references()
    };
    let html = super::parse_markdown(&source, &resolved);
    assert!(
        html.contains("<a href=\"/bob\" class=\"markdown-reference\">@bob</a>"),
        "{html}"
    );
}

#[test]
fn repositories_named_in_code_do_not_hide_a_prose_reference() {
    let code: Vec<String> = (0..10).map(|n| format!("other{n}/repo#1")).collect();
    let source = format!("```\n{}\n```\n\nSee carol/other#9", code.join("\n"));
    let found = references::candidates(&[source.as_str()], false, ("alice", "demo"));
    assert_eq!(
        found.repositories,
        vec![("carol".to_owned(), "other".to_owned())]
    );
}

/// A word with an entity or an escape inside it links nothing, even where its prefix would
/// resolve: `#1&#50;3` renders `#123` and `@ali&#99;e` renders `@alice`.
#[test]
fn a_word_with_an_entity_or_escape_inside_it_stays_plain() {
    let references = References {
        last_number: 20,
        users: vec!["ali".into()],
        ..page_references()
    };
    for source in ["#1&#50;3", "@ali&#99;e", "\\#12", "&#35;12"] {
        let html = super::parse_markdown(source, &references);
        assert!(!html.contains("markdown-reference"), "{source}: {html}");
    }
}

/// The references of a page in `alice/demo` whose latest number is 5, whose viewer can read
/// `carol/other` (latest number 9), and where `bob` exists.
fn page_references() -> References {
    References {
        owner: "alice".into(),
        repo: "demo".into(),
        last_number: 5,
        users: vec!["bob".into()],
        repositories: vec![references::Repository {
            owner: "carol".into(),
            name: "other".into(),
            last_number: 9,
        }],
    }
}

#[test]
fn references_link_only_the_targets_the_page_resolved() {
    let html = super::parse_markdown(
        "See #5, #6, @bob, @dave, carol/other#9, carol/other#10 and alice/demo@abcdef1.",
        &page_references(),
    );
    assert!(
        html.contains("<a href=\"/alice/demo/issues/5\" class=\"markdown-reference\">#5</a>, #6, ")
    );
    assert!(html.contains("<a href=\"/bob\" class=\"markdown-reference\">@bob</a>, @dave, "));
    assert!(html.contains(
            "<a href=\"/carol/other/issues/9\" class=\"markdown-reference\">carol/other#9</a>, carol/other#10 and "
        ));
    assert!(html.contains(
            "<a href=\"/alice/demo/commit/abcdef1\" class=\"markdown-reference markdown-commit\" title=\"abcdef1\">alice/demo@abcdef1</a>."
        ));
}

#[test]
fn a_bare_sha_links_to_this_repository_with_its_full_hash_as_title() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let html = super::parse_markdown(sha, &page_references());
    assert!(html.contains(&format!(
            "<a href=\"/alice/demo/commit/{sha}\" class=\"markdown-reference markdown-commit\" title=\"{sha}\">0123456</a>"
        )));
}

#[test]
fn default_references_link_nothing() {
    let html = parse_markdown("#5 @bob abcdef1 carol/other#9 alice/demo@abcdef1");
    assert!(!html.contains("markdown-reference"));
}

#[test]
fn escaped_and_decoded_references_stay_plain_text() {
    for source in [
        "\\#5",
        "&#35;5",
        "a\\#5",
        "\\@bob",
        "&#64;bob",
        "x\\#5 y&#35;5",
    ] {
        let html = super::parse_markdown(source, &page_references());
        assert!(!html.contains("markdown-reference"), "{source}: {html}");
    }
    let html = super::parse_markdown("\\#1 #5", &page_references());
    assert_eq!(html.matches("markdown-reference").count(), 1, "{html}");
    assert!(
        html.contains("#1 <a href=\"/alice/demo/issues/5\""),
        "{html}"
    );
}

#[test]
fn code_existing_links_images_and_heading_ids_are_left_alone() {
    let html = super::parse_markdown(
        "`#5` and [#5](https://example.com) and ![#5](a.png)\n\n# Fix #5",
        &page_references(),
    );
    assert!(html.contains("<code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">#5</code>"));
    assert!(html.contains("https://example.com\" target=\"_blank\""));
    assert!(html.contains("alt=\"#5\""));
    assert!(html.contains("id=\"user-content-fix-5\""));
    // The heading is the only link to the item: the code, link and image stay as written.
    assert_eq!(
        html.matches("href=\"/alice/demo/issues/5\"").count(),
        1,
        "{html}"
    );
    assert!(
        html.contains("Fix <a href=\"/alice/demo/issues/5\""),
        "{html}"
    );
}

#[test]
fn reference_links_stop_at_the_document_budget() {
    let html = super::parse_markdown(&"#5 ".repeat(1500), &page_references());
    assert_eq!(
        html.matches("class=\"markdown-reference\"").count(),
        MAX_REFERENCE_LINKS
    );
}

/// A run of 100,000 references links up to the budget and keeps every other token as text.
#[test]
fn a_long_run_of_references_links_only_the_budget() {
    let references = References {
        last_number: 1,
        ..page_references()
    };
    let html = super::parse_markdown(&"#1 ".repeat(100_000), &references);
    assert_eq!(
        html.matches("class=\"markdown-reference\"").count(),
        MAX_REFERENCE_LINKS
    );
    assert_eq!(html.matches("#1").count(), 100_000);
}

#[test]
fn references_render_the_same_after_a_round_trip() {
    let references = page_references();
    let json = serde_json::to_string(&references).unwrap();
    let back: References = serde_json::from_str(&json).unwrap();
    let source = "#5 @bob carol/other#9 abcdef1 and #6";
    let first = super::parse_markdown(source, &references);
    assert_eq!(first, super::parse_markdown(source, &back));
    assert_eq!(first, super::parse_markdown(source, &back));
}

/// The HTML inside the paragraph that `text` renders as.
fn parse_inline(text: &str) -> String {
    let html = parse_markdown(text);
    html.strip_prefix("<p class=\"my-3 leading-7 text-foreground\">")
        .and_then(|html| html.strip_suffix("</p>"))
        .unwrap_or_else(|| panic!("not one paragraph: {html}"))
        .to_owned()
}

#[test]
fn empty_or_whitespace_content_renders_nothing() {
    assert_eq!(parse_markdown(""), "");
    assert_eq!(parse_markdown("   \n  "), "");
}

#[test]
fn raw_html_is_escaped_never_rendered() {
    let html = parse_markdown("<script>alert('x')</script>");
    assert!(
        !html.contains("<script>"),
        "raw HTML must not survive: {html}"
    );
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("&#39;x&#39;"));
}

#[test]
fn headings_keep_the_original_classes_for_h1_to_h3_and_h4_to_h6_render() {
    let html = parse_markdown("# One\n## Two\n### Three\n#### Four");
    assert!(html.contains("<h1 id=\"user-content-one\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\">One</h1>"));
    assert!(html.contains("<h2 id=\"user-content-two\" class=\"mb-3 mt-5 border-b border-border pb-2 text-2xl font-semibold text-foreground\">Two</h2>"));
    assert!(html
            .contains("<h3 id=\"user-content-three\" class=\"mb-2 mt-4 text-xl font-semibold text-foreground\">Three</h3>"));
    assert!(html.contains("<h4 id=\"user-content-four\" class=\"mb-2 mt-4 text-lg font-semibold text-foreground\">Four</h4>"));
    let html = parse_markdown("##### Five\n###### Six\n####### Seven");
    assert!(html.contains("<h5 id=\"user-content-five\""));
    assert!(html.contains("<h6 id=\"user-content-six\""));
    // Seven hashes are not a heading.
    assert!(html.contains("<p class=\"my-3 leading-7 text-foreground\">####### Seven</p>"));
}

#[test]
fn fenced_code_is_escaped_and_not_inline_parsed() {
    let html = parse_markdown("```\nlet x = **not bold**;\n```");
    assert!(html.contains("<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\">let x = **not bold**;</code></pre>"));
    assert!(!html.contains("<strong>"));
}

#[test]
fn headings_get_anchors_and_in_page_links_reach_them() {
    let html = parse_markdown(
            "[Install](#installation) [Again](#Usage-2)\n\n## Installation\n\n## Usage\n\n## Usage\n\n### Q&A: \"why\"?",
        );
    assert!(html.contains("<h2 id=\"user-content-installation\""));
    assert!(html.contains("<h2 id=\"user-content-usage\""));
    assert!(html.contains("<h2 id=\"user-content-usage-1\""));
    assert!(html.contains("<h3 id=\"user-content-qa-why\""));
    assert!(html.contains("href=\"#user-content-installation\""));
    assert!(html.contains("href=\"#user-content-usage-2\""));
}

#[test]
fn heading_ids_come_from_visible_text() {
    let html = parse_markdown("## [API](api.md)");
    assert!(html.contains("<h2 id=\"user-content-api\""));
}

#[test]
fn repeated_heading_ids_stay_unique() {
    let html = parse_markdown("## Usage\n\n## Usage\n\n## Usage-1");
    assert!(html.contains("<h2 id=\"user-content-usage\""));
    assert!(html.contains("<h2 id=\"user-content-usage-1\""));
    assert!(html.contains("<h2 id=\"user-content-usage-1-1\""));
}

#[test]
fn repeat_skips_an_id_another_heading_already_took() {
    let html = parse_markdown("# a\n\n# a 1\n\n# a");
    assert!(html.contains("<h1 id=\"user-content-a\""));
    assert!(html.contains("<h1 id=\"user-content-a-1\""));
    assert!(html.contains("<h1 id=\"user-content-a-2\""));
}

#[test]
fn dropped_images_and_sources_do_not_reserve_their_ids() {
    // Neither element is written, so the heading keeps the plain id and the link finds it.
    for source in [
            "<img id=\"usage\" src=\"javascript:x\">\n\n# Usage\n\n[go](#usage)",
            "<picture><source id=\"usage\" srcset=\"javascript:x\"></picture>\n\n# Usage\n\n[go](#usage)",
        ] {
            let html = parse_markdown(source);
            assert!(html.contains("<h1 id=\"user-content-usage\""), "{html}");
            assert!(html.contains("href=\"#user-content-usage\""), "{html}");
        }
}

#[test]
fn many_identical_headings_render_quickly_with_unique_ids() {
    let started = std::time::Instant::now();
    let html = parse_markdown(&"# x\n".repeat(20_000));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(html.contains("<h1 id=\"user-content-x\""));
    assert!(html.contains("<h1 id=\"user-content-x-19999\""));
    let ids: HashSet<&str> = html
        .split("<h1 id=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(ids.len(), 20_000);
}

#[test]
fn unterminated_fence_still_closes_at_end() {
    let html = parse_markdown("```\nno closing fence");
    assert!(html.contains(
        "<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\">"
    ));
}

#[test]
fn lists_group_consecutive_items() {
    let html = parse_markdown("- one\n- two\n\nafter");
    assert!(html.contains("<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>one</li>\n<li>two</li>\n</ul>"));
    assert!(html.contains("<p class=\"my-3 leading-7 text-foreground\">after</p>"));
}

#[test]
fn inline_transforms_match_the_original_order() {
    assert_eq!(
        parse_inline("**bold**"),
        "<strong class=\"font-semibold text-foreground\">bold</strong>"
    );
    assert_eq!(parse_inline("*it*"), "<em class=\"italic\">it</em>");
    assert_eq!(
            parse_inline("`code`"),
            "<code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">code</code>"
        );
    assert_eq!(
            parse_inline("![alt](pic.png)"),
            "<img src=\"pic.png\" alt=\"alt\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" />"
        );
    assert_eq!(
            parse_inline("[text](https://example.com)"),
            "<a href=\"https://example.com\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">text</a>"
        );
}

#[test]
fn never_reprocesses_generated_markup() {
    let html = parse_inline("`**x**`");
    assert!(html.starts_with("<code class="));
    assert!(html.contains("**x**"));
    assert!(!html.contains("<strong"));
}

#[test]
fn rejects_unsafe_urls_and_attribute_breakouts() {
    for url in [
        "javascript:alert",
        "data:text/html,bad",
        "vbscript:bad",
        "//evil.example/x",
        "java&#x73;cript:bad",
        "java&#115;cript:bad",
        "java&#X73;cript:bad",
        "javascript&#58;bad",
        "java&#9;script:bad",
        "java&#10;script:bad",
        "java\tscript:bad",
        "\\\\evil.example/x",
    ] {
        let html = parse_markdown(&format!("[click]({url}) ![image]({url})"));
        println!("denied URL {url}; HTML={html}");
        assert!(!html.contains("href="), "unsafe link: {url}; HTML={html}");
        assert!(!html.contains("src="), "unsafe image: {url}; HTML={html}");
    }
    let html = parse_inline("[<script>](https://example.org/\"onmouseover=\"bad)");
    assert!(!html.contains("<script>"));
    assert!(!html.contains("\"onmouseover="));
    assert!(parse_markdown("![raw](/owner/repo/raw/main/a.png)")
        .contains("src=\"/owner/repo/raw/main/a.png\""));
}

#[test]
fn inline_delimiters_require_content() {
    assert_eq!(parse_inline("``"), "``");
    assert_eq!(parse_inline("**"), "**");
    // An empty destination is not a link; its empty label is all that is left.
    assert_eq!(parse_inline("[]()"), "");
    assert_eq!(parse_inline("[x]()"), "x");
    assert_eq!(parse_inline("![](x)"), "<img src=\"x\" alt=\"\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" />");
}

/// Rendering takes time proportional to the source: `unit` repeated `count` times and
/// four times as often differ by far less than the 16 a quadratic scan would show.
/// The best of three runs keeps a loaded machine from failing the comparison.
fn assert_linear(unit: &str, count: usize) {
    for document in [false, true] {
        let time = |count: usize| {
            let source = unit.repeat(count);
            (0..3)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let _ = parse_inner(&source, document);
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        let small = time(count).max(std::time::Duration::from_millis(50));
        let large = time(count * 4);
        assert!(
            large < small * 10 && large < std::time::Duration::from_secs(8),
            "{unit:?} x{count}: {small:?} then {large:?}"
        );
    }
}

fn parse_inner(source: &str, document: bool) -> String {
    bounded_markdown(source, document).unwrap_or_default()
}

#[test]
fn large_unclosed_labels_and_urls_stay_literal() {
    for count in [4_096, 65_536, 262_144] {
        for prefix in ["[", "![", "[x]("] {
            let source = prefix.repeat(count);
            assert_eq!(parse_inline(&source), source);
        }
    }
    let started = std::time::Instant::now();
    let source = format!("{} [ok](https://example.invalid)", "![".repeat(65_536));
    let html = parse_inline(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert_eq!(html.matches("<a ").count(), 1);
    assert!(html.contains("href=\"https://example.invalid\""));
}

#[test]
fn overlapping_rejected_urls_do_not_rescan_their_shared_suffix() {
    let prefix = "[label](".repeat(65_536);
    for suffix in [
        "javascript:bad)",
        "bad url)",
        "java\\bad)",
        "java&#115;cript:bad)",
    ] {
        let started = std::time::Instant::now();
        let html = parse_markdown(&format!("{prefix}{suffix}"));
        assert!(started.elapsed() < std::time::Duration::from_secs(8));
        assert!(!html.contains("href="), "{suffix}");
    }
    let started = std::time::Instant::now();
    let html = parse_inline(&format!("{prefix}HtTpS://example.invalid)"));
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
    assert!(html.starts_with(&"[label](".repeat(65_535)));
    assert_eq!(html.matches("<a ").count(), 1);
    assert!(html.ends_with("class=\"text-primary hover:underline\">label</a>"));
}

#[test]
fn adversarial_sources_render_in_linear_time() {
    for unit in [
        "[",
        "![",
        "](",
        "*",
        "_",
        "`",
        "~~",
        "|",
        ">",
        "> ",
        "- ",
        "1. ",
        "[x]: y\n",
        "[^1]",
        "[a](",
        "*a ",
        "_a ",
        "`a",
        "| a |\n",
        "|-|\n",
        "<",
        "<a ",
        "&amp",
        "\\",
        "http://",
        "(",
        ")",
        "- [ ] ",
        "a\n    b\n",
        "# ",
        "---\n",
        "```\n",
        "[a]: <b>\n",
        "!!!",
        "<!--\n",
        "<div>\n",
        "</div>\n",
        "<kbd>",
        "&#",
    ] {
        assert_linear(unit, 8_000);
    }
}

#[test]
fn raw_html_never_becomes_a_tag() {
    let allowed = ALLOWED_ELEMENTS;
    for source in [
        "<script>alert(1)</script>",
        "text <script>alert(1)</script> text",
        "<img src=x onerror=alert(1)>",
        "a <img src=x onerror=alert(1)> b",
        "<iframe src=\"https://evil.example\"></iframe>",
        "<div>\n<b>block</b>\n</div>",
        "<div onclick=\"x()\">\n\ntext\n\n</div>",
        "*<b onmouseover=x>emphasis</b>*",
        "**<i>strong</i>**",
        "| a | b |\n|---|---|\n| <u>x</u> | <img src=y onerror=z> |",
        "- <div>item</div>\n- <input type=checkbox>",
        "> <style>*{display:none}</style>",
        "# <svg onload=x>",
        "<!-- comment --> <?php ?> <![CDATA[x]]>",
        "<input type=\"checkbox\">",
        "<a href=\"javascript:alert(1)\">x</a>",
    ] {
        for html in [parse_markdown(source), parse_document(source)] {
            let mut rest = html.as_str();
            while let Some((_, after)) = rest.split_once('<') {
                let name: String = after
                    .trim_start_matches('/')
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect();
                assert!(
                    allowed.contains(&name.as_str()),
                    "<{name}> from {source:?}: {html}"
                );
                rest = after;
            }
            assert!(!html.contains("<input"), "{source:?}: {html}");
            for handler in ["onerror", "onclick", "onmouseover"] {
                assert!(!html.contains(handler), "{source:?}: {html}");
            }
        }
    }
}

#[test]
fn unsafe_url_schemes_never_become_links_or_images() {
    for source in [
        "[x](javascript:alert(1))",
        "[x](JaVaScRiPt:alert(1))",
        "[x](&#115;cript:alert(1))",
        "[x](&#x6A;avascript:alert(1))",
        "[x](java&Tab;script:alert(1))",
        "[x](data:text/html;base64,PHNjcmlwdD4=)",
        "[x](vbscript:msgbox)",
        "[x]\n\n[x]: javascript:alert(1)",
        "[x][r]\n\n[r]: JAVASCRIPT:alert(1)",
        "<javascript:alert(1)>",
        "<data:text/html,x>",
        "![x](javascript:alert(1))",
        "![x](data:image/svg+xml,<svg onload=x>)",
        "![x][r]\n\n[r]: data:text/html,x",
        "![x](mailto:a@example.invalid)",
        "[x](//evil.example/x) ![x](//evil.example/x)",
        "[x](\\\\evil.example)",
        "javascript:alert(1) data:text/html,x",
    ] {
        for html in [parse_markdown(source), parse_document(source)] {
            let lower = html.to_ascii_lowercase();
            for bad in ["javascript:", "data:", "vbscript:", "//"] {
                for attribute in ["href", "src"] {
                    assert!(
                        !lower.contains(&format!("{attribute}=\"{bad}")),
                        "{source:?} -> {html}"
                    );
                }
            }
            assert!(!lower.contains("src=\"mailto"), "{source:?} -> {html}");
        }
    }
    let html = parse_markdown(
            "[a](https://example.invalid) [b](http://example.invalid/x) [c](rel/x.md) [d](mailto:a@example.invalid) <https://auto.invalid> <me@example.invalid> [e][r]\n\n[r]: https://ref.invalid",
        );
    for href in [
        "https://example.invalid",
        "http://example.invalid/x",
        "rel/x.md",
        "mailto:a@example.invalid",
        "https://auto.invalid",
        "mailto:me@example.invalid",
        "https://ref.invalid",
    ] {
        assert!(
            html.contains(&format!("<a href=\"{href}\" target=\"_blank\"")),
            "{href}: {html}"
        );
    }
}

#[test]
fn unsafe_link_keeps_its_label_and_drops_the_tag() {
    assert_eq!(
        parse_inline("[**label**](javascript:alert(1)) end"),
        "<strong class=\"font-semibold text-foreground\">label</strong> end"
    );
    assert_eq!(parse_inline("![alt text](javascript:x)"), "alt text");
}

#[test]
fn link_and_image_titles_are_escaped() {
    let html = parse_markdown("[x](https://example.invalid \"a \\\"b\\\" <c>\") ![i](p.png \"t\")");
    assert!(html.contains(
        "class=\"text-primary hover:underline\" title=\"a &quot;b&quot; &lt;c&gt;\">x</a>"
    ));
    assert!(html.contains("alt=\"i\" title=\"t\" loading=\"lazy\""));
}

#[test]
fn nesting_deeper_than_the_limit_shows_the_preview() {
    // Lists and quotes close at the end of the source; spans need their closing marker.
    for (open, close) in [
        ("> ", ""),
        ("- ", ""),
        ("1. ", ""),
        ("*a ", " a*"),
        ("**a ", " a**"),
    ] {
        let nested = |depth: usize| open.repeat(depth) + "x" + &close.repeat(depth);
        assert!(
            bounded_markdown(&nested(MAX_NESTING), false).is_some(),
            "{open:?}"
        );
        let over = nested(MAX_NESTING + 1);
        assert!(bounded_markdown(&over, false).is_none(), "{open:?}");
        assert!(parse_markdown(&over).contains("showing a plain-text preview"));
    }
}

#[test]
fn deep_spans_fall_back_before_raw_links_are_checked() {
    // Thousands of spans are open around the raw links, so the page takes the over-limit path.
    let source = format!(
        "{}x{}y{}",
        "**".repeat(3_000),
        "<a>".repeat(3_000),
        "**".repeat(3_000)
    );
    assert!(bounded_markdown(&source, false).is_none());
    assert!(parse_markdown(&source).contains("showing a plain-text preview"));
}

#[test]
fn commonmark_destinations_encode_spaces_and_srcsets_preserve_url_commas() {
    let html = parse_document("[Guide](<docs/My Guide.md>) ![logo](<images/my logo.png>)");
    assert!(html.contains("href=\"docs/My%20Guide.md\""), "{html}");
    assert!(html.contains("src=\"images/my%20logo.png\""), "{html}");
    let base = LinkBase {
        blob: "/a/r/blob/main".into(),
        raw: "/a/r/raw/main".into(),
        ..Default::default()
    };
    let srcset = "https://cdn.test/image?crop=10,20 2x, small.png 1x";
    assert_eq!(
        base.resolve_srcset(srcset),
        "https://cdn.test/image?crop=10,20 2x, /a/r/raw/main/small.png 1x"
    );
    let html = parse_document("<picture><source srcset=\"https://cdn.test/image?crop=10,20 2x\"><img src=\"small.png\"></picture>");
    assert!(
        html.contains("srcset=\"https://cdn.test/image?crop=10,20 2x\""),
        "{html}"
    );
    assert!(!safe_url("javascript:alert(1)", false));
    assert!(!safe_url("docs/a\tb", false));
}

#[test]
fn sources_over_the_limit_show_the_preview() {
    let source = "a".repeat(MAX_SOURCE_BYTES + 1);
    assert!(bounded_markdown(&source, true).is_none());
    assert!(parse_document(&source).contains("showing a plain-text preview"));
    assert!(bounded_markdown(&source[1..], true).is_some());
}

#[test]
fn highlighting_stops_at_the_block_and_document_budgets() {
    // One line of spaces is one token, so the output stays small.
    let block = |size: usize| format!("```rust\n{}\n```\n\n", " ".repeat(size));
    let count = |html: &str| {
        html.split("<pre ")
            .filter(|b| b.contains("<span style="))
            .count()
    };
    assert_eq!(count(&parse_markdown(&block(1_024))), 1);
    assert_eq!(
        count(&parse_markdown(&block(MAX_HIGHLIGHTED_BLOCK_BYTES))),
        1
    );
    assert_eq!(
        count(&parse_markdown(&block(MAX_HIGHLIGHTED_BLOCK_BYTES + 1))),
        0
    );
    let many = block(60 * 1024).repeat(5);
    assert_eq!(count(&parse_markdown(&many)), 4);
    // A block over budget is still escaped code.
    let html = parse_markdown(&format!(
        "```rust\n{}\n```",
        "<".repeat(MAX_HIGHLIGHTED_BLOCK_BYTES + 1)
    ));
    assert!(html.contains("&lt;&lt;&lt;") && !html.contains("<span style="));
}

#[test]
fn fenced_code_uses_the_language_of_its_info_string() {
    let html = parse_markdown("```rust,no_run\nfn main() {}\n```\n\n```unknown-lang\n<x>\n```");
    assert!(html.contains("<span style=\"color:var(--repo-code-"));
    assert!(html.contains("&lt;x&gt;</code></pre>"));
    assert!(!html.contains("language-"));
}

#[test]
fn bare_urls_become_links_outside_code_and_links() {
    let html = parse_markdown(
            "see https://example.invalid/a_b?x=1&y=2. And (http://example.invalid/p_(q)) or `https://code.invalid` or [https://label.invalid](https://target.invalid) xhttps://no.invalid https:// end",
        );
    assert!(html.contains("<a href=\"https://example.invalid/a_b?x=1&amp;y=2\" target=\"_blank\""));
    assert!(html.contains(">https://example.invalid/a_b?x=1&amp;y=2</a>. And"));
    assert!(html.contains("<a href=\"http://example.invalid/p_(q)\""));
    assert!(html.contains("</a>) or"));
    assert!(html.contains("rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">https://code.invalid</code>"));
    assert_eq!(html.matches("<a ").count(), 3, "{html}");
    assert!(html.contains("xhttps://no.invalid https:// end"));
}

#[test]
fn bare_urls_in_headings_do_not_change_the_id() {
    let html = parse_markdown("# See https://example.invalid/x");
    assert!(html.contains("<h1 id=\"user-content-see-httpsexampleinvalidx\""));
}

#[test]
fn alerts_render_with_a_title_and_plain_quotes_do_not() {
    let html = parse_markdown("> [!WARNING]\n> mind the gap\n\n> quoted");
    assert!(html.contains("<blockquote class=\"markdown-alert markdown-alert-warning\"><p class=\"markdown-alert-title\">Warning</p>"));
    assert!(html.contains("mind the gap</p>\n</blockquote>"));
    assert!(html.contains(
        "<blockquote>\n<p class=\"my-3 leading-7 text-foreground\">quoted</p>\n</blockquote>"
    ));
}

#[test]
fn tables_keep_alignment_and_escape_cells() {
    let html = parse_markdown("| a | b | c |\n|:--|:-:|--:|\n| <x> | **y** | z |");
    assert!(html.contains("<table>\n<thead><tr><th class=\"text-left\">a</th><th class=\"text-center\">b</th><th class=\"text-right\">c</th></tr></thead>"));
    assert!(
        html.contains("<td class=\"text-left\">&lt;x&gt;</td><td class=\"text-center\"><strong")
    );
    assert!(html.contains("</tbody></table>"));
}

#[test]
fn nested_and_ordered_lists_keep_their_structure() {
    let html = parse_markdown("1. one\n2. two\n   - inner\n\n3. three\n\n5. five");
    assert!(html.contains("<ol class=\"my-3 list-decimal space-y-1 pl-5 text-foreground\">"));
    assert!(html.contains("<ul class=\"my-3 list-disc"));
    assert!(parse_markdown("3. a\n4. b").contains("<ol start=\"3\" class="));
}

#[test]
fn soft_breaks_follow_the_mode_and_footnotes_need_a_document() {
    assert_eq!(
        parse_markdown("a\nb"),
        "<p class=\"my-3 leading-7 text-foreground\">a<br>b</p>"
    );
    assert_eq!(
        parse_document("a\nb"),
        "<p class=\"my-3 leading-7 text-foreground\">a b</p>"
    );
    let source = "text[^n]\n\n[^n]: note";
    let document = parse_document(source);
    assert!(document
        .contains("<sup class=\"footnote-reference\"><a href=\"#user-content-fn:1\">1</a></sup>"));
    assert!(document.contains("<div class=\"footnote-definition\" id=\"user-content-fn:1\">"));
    assert!(!parse_markdown(source).contains("footnote"));
}

#[test]
fn footnote_labels_match_with_unicode_case_folding() {
    for (reference, definition) in [
        ("a", "A"),
        ("Note", "nOTE"),
        ("é", "É"),
        ("Straße", "STRASSE"),
    ] {
        let html = parse_document(&format!(
            "text[^{reference}]\n\n[^{definition}]: Footnote text."
        ));
        assert!(
            html.contains("<a href=\"#user-content-fn:1\">1</a>"),
            "{reference}: {html}"
        );
        assert!(
            html.contains("id=\"user-content-fn:1\""),
            "{reference}: {html}"
        );
        assert!(!html.contains("fn:2"), "{reference}: {html}");
    }
}

#[test]
fn multiline_heading_ids_keep_the_word_break() {
    // A soft break and a backslash hard break inside setext headings.
    let source = "[go](#hello-world) [hard](#a-b-c)\n\nHello\nworld\n---\n\na\\\nb\nc\n===";
    for html in [parse_document(source), parse_markdown(source)] {
        assert!(
            html.contains("<h2 id=\"user-content-hello-world\""),
            "{html}"
        );
        assert!(html.contains("<h1 id=\"user-content-a-b-c\""), "{html}");
        assert!(
            html.contains("href=\"#user-content-hello-world\""),
            "{html}"
        );
        assert!(html.contains("href=\"#user-content-a-b-c\""), "{html}");
    }
    // The displayed breaks follow the mode, as in paragraphs.
    assert!(parse_document(source).contains(">Hello world</h2>"));
    assert!(parse_markdown(source).contains(">Hello<br>world</h2>"));
}

#[test]
fn code_blocks_and_rules() {
    let html = parse_markdown("    indented <b>\n\n---\n\ntext\n---");
    assert!(html
        .contains("<code class=\"font-mono text-sm text-foreground\">indented &lt;b&gt;</code>"));
    assert!(html.contains("<hr class=\"my-6 border-border\">"));
    assert!(html.contains("<h2 id=\"user-content-text\""));
    assert!(parse_markdown("~~~\n**x**\n~~~").contains(">**x**</code></pre>"));
}

#[test]
fn heading_writer_output_is_one_line_per_heading() {
    let html = parse_markdown("## *a* `b`\n\ntext");
    assert!(html.starts_with("<h2 id=\"user-content-a-b\" class="));
    assert!(html.contains("><em class=\"italic\">a</em> <code class="));
}

#[test]
fn every_document_heading_with_an_id_has_one_empty_permalink_to_it() {
    let html = parse_document("# One\n\n## Usage\n\n## Usage\n\n###### Deep\n\n### !!!\n\ntext");
    assert_eq!(html.matches("class=\"heading-permalink\"").count(), 4);
    for id in [
        "user-content-one",
        "user-content-usage",
        "user-content-usage-1",
        "user-content-deep",
    ] {
        assert!(html.contains(&format!(
                "<a class=\"heading-permalink\" href=\"#{id}\" aria-label=\"Permalink to this section\"></a>"
            )));
    }
    // A heading with no usable text has no id and nothing to link to.
    assert!(html.contains("<h3 class="));
}

#[test]
fn comments_get_no_permalinks_but_keep_their_ids() {
    // Comments and descriptions share a page, so their ids repeat; a permalink would
    // jump to the first one.
    let html = parse_markdown("## Details\n");
    assert!(!html.contains("heading-permalink"));
    assert!(html.contains("id=\"user-content-details\""));
}

#[test]
fn permalinks_carry_no_source_markup_and_survive_link_rewriting() {
    let source = "## <img src=x onerror=alert(1)> \"quoted\" [x](javascript:alert(1)) <script>\n";
    let html = parse_document(source);
    // The image is an allowed raw element now, but its event handler is removed.
    assert!(!html.contains("onerror"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("href=\"javascript:"));
    let permalink = html
        .split("<a class=\"heading-permalink\" href=\"")
        .nth(1)
        .unwrap()
        .split_once("\" aria-label=\"Permalink to this section\"></a>")
        .unwrap()
        .0;
    assert!(permalink.starts_with("#user-content-"));
    assert!(permalink
        .chars()
        .all(|c| c == '#' || c == '-' || c == '_' || c.is_alphanumeric()));
    assert!(html.contains(&format!("id=\"{}\"", &permalink[1..])));
    // A repository file keeps the permalink as an in-page link that opens in place.
    let rewritten = base().rewrite(&html).unwrap();
    assert!(rewritten.contains(&format!(
            "<a class=\"heading-permalink\" href=\"{permalink}\" aria-label=\"Permalink to this section\"></a>"
        )));
}

#[test]
fn permalink_icon_does_not_depend_on_data_urls() {
    // The page's CSP allows images only from this site and over http(s).
    assert!(!include_str!("native.css").contains("data:"));
}

/// A README with one of everything the renderer supports.
const README: &str = "\
# Project *Title*
Setext heading
==============
Intro with **bold**, _emphasis_, ~~strike~~, `code`, a [link](docs/guide.md#top \"Guide\"), a [reference][ref], <https://auto.invalid>, <me@example.invalid> and bare https://bare.invalid/x_y.\\
Hard break, then ![logo](img/logo.png) and a footnote.[^1]

> quoted *text*

> [!TIP]
> Alert body

## Lists
- one
  - nested
- [x] done
- [ ] open

3. three
4. four

| Name | Qty |
|:-----|----:|
| a    | 1   |

```rust
fn main() {}
```

    indented

---

#### Level four
<div>raw html</div>

[ref]: https://example.invalid/ref
[^1]: Footnote text.
";

#[test]
fn readme_renders_every_construct() {
    let html = parse_document(README);
    for expected in [
            "<h1 id=\"user-content-project-title\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\"><a class=\"heading-permalink\" href=\"#user-content-project-title\" aria-label=\"Permalink to this section\"></a>Project <em class=\"italic\">Title</em></h1>",
            "<h1 id=\"user-content-setext-heading\"",
            "<strong class=\"font-semibold text-foreground\">bold</strong>",
            "<em class=\"italic\">emphasis</em>",
            "<del>strike</del>",
            "<a href=\"docs/guide.md#top\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\" title=\"Guide\">link</a>",
            "<a href=\"https://example.invalid/ref\" target=\"_blank\"",
            "<a href=\"https://auto.invalid\" target=\"_blank\"",
            "<a href=\"mailto:me@example.invalid\" target=\"_blank\"",
            "<a href=\"https://bare.invalid/x_y\" target=\"_blank\"",
            "<br>",
            "<img src=\"img/logo.png\" alt=\"logo\" loading=\"lazy\"",
            "<sup class=\"footnote-reference\"><a href=\"#user-content-fn:1\">1</a></sup>",
            "<blockquote>\n<p class=\"my-3 leading-7 text-foreground\">quoted <em class=\"italic\">text</em></p>\n</blockquote>",
            "<blockquote class=\"markdown-alert markdown-alert-tip\"><p class=\"markdown-alert-title\">Tip</p>",
            "<h2 id=\"user-content-lists\"",
            "<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>one\n<ul class=",
            "<li><input type=\"checkbox\" disabled checked class=\"mr-2\">done</li>",
            "<li><input type=\"checkbox\" disabled class=\"mr-2\">open</li>",
            "<ol start=\"3\" class=\"my-3 list-decimal",
            "<th class=\"text-left\">Name</th><th class=\"text-right\">Qty</th>",
            "<td class=\"text-left\">a</td><td class=\"text-right\">1</td>",
            "<span style=\"color:var(--repo-code-",
            ">indented</code></pre>",
            "<hr class=\"my-6 border-border\">",
            "<h4 id=\"user-content-level-four\"",
            "<div>raw html</div>",
            "<div class=\"footnote-definition\" id=\"user-content-fn:1\">",
        ] {
            assert!(html.contains(expected), "missing {expected:?} in:\n{html}");
        }
}

#[test]
fn rendering_is_deterministic() {
    for document in [false, true] {
        let first = parse_inner(README, document);
        for _ in 0..8 {
            assert_eq!(parse_inner(README, document), first);
        }
    }
}

#[test]
fn comment_snapshot() {
    assert_eq!(parse_inner(README, false), GOLDEN_COMMENT);
}

#[test]
fn document_snapshot() {
    assert_eq!(parse_inner(README, true), GOLDEN_DOCUMENT);
}

const GOLDEN_COMMENT: &str =
        "<h1 id=\"user-content-project-title\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\">Project <em class=\"italic\">Title</em></h1>\n<h1 id=\"user-content-setext-heading\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\">Setext heading</h1>\n<p class=\"my-3 leading-7 text-foreground\">Intro with <strong class=\"font-semibold text-foreground\">bold</strong>, <em class=\"italic\">emphasis</em>, <del>strike</del>, <code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">code</code>, a <a href=\"docs/guide.md#top\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\" title=\"Guide\">link</a>, a <a href=\"https://example.invalid/ref\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">reference</a>, <a href=\"https://auto.invalid\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">https://auto.invalid</a>, <a href=\"mailto:me@example.invalid\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">me@example.invalid</a> and bare <a href=\"https://bare.invalid/x_y\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">https://bare.invalid/x_y</a>.<br>Hard break, then <img src=\"img/logo.png\" alt=\"logo\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" /> and a footnote.[^1]</p>\n<blockquote>\n<p class=\"my-3 leading-7 text-foreground\">quoted <em class=\"italic\">text</em></p>\n</blockquote>\n<blockquote class=\"markdown-alert markdown-alert-tip\"><p class=\"markdown-alert-title\">Tip</p>\n<p class=\"my-3 leading-7 text-foreground\">Alert body</p>\n</blockquote>\n<h2 id=\"user-content-lists\" class=\"mb-3 mt-5 border-b border-border pb-2 text-2xl font-semibold text-foreground\">Lists</h2>\n<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>one\n<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>nested</li>\n</ul></li>\n<li><input type=\"checkbox\" disabled checked class=\"mr-2\">done</li>\n<li><input type=\"checkbox\" disabled class=\"mr-2\">open</li>\n</ul>\n<ol start=\"3\" class=\"my-3 list-decimal space-y-1 pl-5 text-foreground\">\n<li>three</li>\n<li>four</li>\n</ol>\n<table>\n<thead><tr><th class=\"text-left\">Name</th><th class=\"text-right\">Qty</th></tr></thead>\n<tbody>\n<tr><td class=\"text-left\">a</td><td class=\"text-right\">1</td></tr>\n</tbody></table>\n<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\"><span style=\"color:var(--repo-code-keyword)\">fn</span><span style=\"color:var(--repo-code-text)\"> </span><span style=\"color:var(--repo-code-function)\">main</span><span style=\"color:var(--repo-code-text)\">(</span><span style=\"color:var(--repo-code-text)\">)</span><span style=\"color:var(--repo-code-text)\"> </span><span style=\"color:var(--repo-code-text)\">{</span><span style=\"color:var(--repo-code-text)\">}</span></code></pre>\n<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\">indented</code></pre>\n<hr class=\"my-6 border-border\">\n<h4 id=\"user-content-level-four\" class=\"mb-2 mt-4 text-lg font-semibold text-foreground\">Level four</h4>\n<div>raw html</div>\n<p class=\"my-3 leading-7 text-foreground\">[^1]: Footnote text.</p>";
const GOLDEN_DOCUMENT: &str =
        "<h1 id=\"user-content-project-title\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\"><a class=\"heading-permalink\" href=\"#user-content-project-title\" aria-label=\"Permalink to this section\"></a>Project <em class=\"italic\">Title</em></h1>\n<h1 id=\"user-content-setext-heading\" class=\"mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0\"><a class=\"heading-permalink\" href=\"#user-content-setext-heading\" aria-label=\"Permalink to this section\"></a>Setext heading</h1>\n<p class=\"my-3 leading-7 text-foreground\">Intro with <strong class=\"font-semibold text-foreground\">bold</strong>, <em class=\"italic\">emphasis</em>, <del>strike</del>, <code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">code</code>, a <a href=\"docs/guide.md#top\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\" title=\"Guide\">link</a>, a <a href=\"https://example.invalid/ref\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">reference</a>, <a href=\"https://auto.invalid\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">https://auto.invalid</a>, <a href=\"mailto:me@example.invalid\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">me@example.invalid</a> and bare <a href=\"https://bare.invalid/x_y\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">https://bare.invalid/x_y</a>.<br>Hard break, then <img src=\"img/logo.png\" alt=\"logo\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" /> and a footnote.<sup class=\"footnote-reference\"><a href=\"#user-content-fn:1\">1</a></sup></p>\n<blockquote>\n<p class=\"my-3 leading-7 text-foreground\">quoted <em class=\"italic\">text</em></p>\n</blockquote>\n<blockquote class=\"markdown-alert markdown-alert-tip\"><p class=\"markdown-alert-title\">Tip</p>\n<p class=\"my-3 leading-7 text-foreground\">Alert body</p>\n</blockquote>\n<h2 id=\"user-content-lists\" class=\"mb-3 mt-5 border-b border-border pb-2 text-2xl font-semibold text-foreground\"><a class=\"heading-permalink\" href=\"#user-content-lists\" aria-label=\"Permalink to this section\"></a>Lists</h2>\n<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>one\n<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">\n<li>nested</li>\n</ul></li>\n<li><input type=\"checkbox\" disabled checked class=\"mr-2\">done</li>\n<li><input type=\"checkbox\" disabled class=\"mr-2\">open</li>\n</ul>\n<ol start=\"3\" class=\"my-3 list-decimal space-y-1 pl-5 text-foreground\">\n<li>three</li>\n<li>four</li>\n</ol>\n<table>\n<thead><tr><th class=\"text-left\">Name</th><th class=\"text-right\">Qty</th></tr></thead>\n<tbody>\n<tr><td class=\"text-left\">a</td><td class=\"text-right\">1</td></tr>\n</tbody></table>\n<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\"><span style=\"color:var(--repo-code-keyword)\">fn</span><span style=\"color:var(--repo-code-text)\"> </span><span style=\"color:var(--repo-code-function)\">main</span><span style=\"color:var(--repo-code-text)\">(</span><span style=\"color:var(--repo-code-text)\">)</span><span style=\"color:var(--repo-code-text)\"> </span><span style=\"color:var(--repo-code-text)\">{</span><span style=\"color:var(--repo-code-text)\">}</span></code></pre>\n<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\">indented</code></pre>\n<hr class=\"my-6 border-border\">\n<h4 id=\"user-content-level-four\" class=\"mb-2 mt-4 text-lg font-semibold text-foreground\"><a class=\"heading-permalink\" href=\"#user-content-level-four\" aria-label=\"Permalink to this section\"></a>Level four</h4>\n<div>raw html</div>\n<div class=\"footnote-definition\" id=\"user-content-fn:1\"><sup class=\"footnote-definition-label\">1</sup>\n<p class=\"my-3 leading-7 text-foreground\">Footnote text.</p></div>";

#[test]
fn emoji_shortcodes_render_known_codes_and_keep_unknown_ones() {
    assert_eq!(parse_inline(":tada: :+1: :-1: :100:"), "🎉 👍 👎 💯");
    assert_eq!(
        parse_inline(":not_a_code: :tada :: :: :TADA:"),
        ":not_a_code: :tada :: :: :TADA:"
    );
    assert_eq!(parse_inline("a:b:c 12:30:45"), "a:b:c 12:30:45");
}

#[test]
fn emoji_shortcodes_next_to_punctuation_and_markup() {
    assert_eq!(parse_inline(":tada:!"), "🎉!");
    assert_eq!(parse_inline("(:rocket:)"), "(🚀)");
    assert_eq!(parse_inline(":fire::fire:"), "🔥🔥");
    assert_eq!(
        parse_inline("**:warning: careful**"),
        "<strong class=\"font-semibold text-foreground\">⚠\u{FE0F} careful</strong>"
    );
    // A shortcode name is bounded, so a long run of name characters is just text.
    let long = format!(":{}:", "a".repeat(100));
    assert_eq!(parse_inline(&long), long);
}

#[test]
fn emoji_shortcodes_stay_literal_in_code_urls_and_heading_ids() {
    let html = parse_markdown(
            "`:tada:`\n\n```\n:rocket:\n```\n\nSee https://example.invalid/:tada:/x and [:fire: label](https://example.invalid/:fire:) ![:tada:](a.png)\n\n# Ship :rocket:",
        );
    assert!(html.contains("text-foreground\">:tada:</code>"));
    assert!(html.contains(">:rocket:</code></pre>"));
    assert!(html.contains("<a href=\"https://example.invalid/:tada:/x\""));
    assert!(html.contains(">https://example.invalid/:tada:/x</a>"));
    assert!(html.contains("<a href=\"https://example.invalid/:fire:\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">🔥 label</a>"));
    assert!(html.contains("alt=\":tada:\""));
    assert!(html.contains("<h1 id=\"user-content-ship-rocket\""));
    assert!(html.contains(">Ship 🚀</h1>"));
    let raw = parse_markdown("Example <code>:rocket:</code>");
    assert!(raw.contains(":rocket:</code>"), "{raw}");
}

#[test]
fn emoji_table_is_sorted_and_names_are_valid() {
    assert!(EMOJI.windows(2).all(|pair| pair[0].0 < pair[1].0));
    for (name, emoji) in EMOJI {
        assert!(!name.is_empty() && name.len() <= MAX_EMOJI_NAME, "{name}");
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_+-".contains(&b)));
        assert!(!emoji.is_empty());
        assert_eq!(
            emoji_at(&format!(":{name}:"), 0),
            Some((name.len() + 2, *emoji))
        );
    }
}

#[test]
fn emoji_and_url_scanning_stay_linear() {
    for unit in [
        ":tada:",
        ":",
        ":a",
        "::",
        ":tada:http://x.invalid/:a:",
        "http://x ",
        ":+1",
    ] {
        assert_linear(unit, 8_000);
    }
}

#[test]
fn unicode_and_delimiter_precedence_keep_the_original_markup() {
    assert_eq!(
        parse_inline("***雪**"),
        "*<strong class=\"font-semibold text-foreground\">雪</strong>"
    );
    assert_eq!(parse_inline("**雪*"), "*<em class=\"italic\">雪</em>");
    assert_eq!(parse_inline("![**雪**](画像.png)"), "<img src=\"画像.png\" alt=\"雪\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" />");
    assert_eq!(parse_inline("[é](../雪?q=✓)"), "<a href=\"../雪?q=✓\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">é</a>");
    assert_eq!(parse_inline("![mail](mailto:user@example.invalid)"), "mail");
    assert!(!parse_inline("[unicode space](https://example.invalid/\u{2003}x)").contains("href="));
}

/// Every element the renderer may write.
const ALLOWED_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "s",
    "samp",
    "small",
    "source",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
    "var",
];

/// The attributes of a highlighting span: the token colors that `themed_highlight` maps to
/// theme variables.
const HIGHLIGHT_ATTRIBUTES: [&str; 8] = [
    " style=\"color:var(--repo-code-comment)\"",
    " style=\"color:var(--repo-code-string)\"",
    " style=\"color:var(--repo-code-constant)\"",
    " style=\"color:var(--repo-code-keyword)\"",
    " style=\"color:var(--repo-code-function)\"",
    " style=\"color:var(--repo-code-variable)\"",
    " style=\"color:var(--repo-code-tag)\"",
    " style=\"color:var(--repo-code-text)\"",
];

/// Checks rendered HTML the way a reviewer would: allowed elements only, no event handler,
/// only safe addresses, ids under `user-content-`, no `input`, and balanced tags.
fn assert_safe_html(source: &str, html: &str) {
    const VOID: [&str; 4] = ["br", "hr", "img", "source"];
    let mut open: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('/') {
            let name: String = after
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            assert_eq!(
                open.pop().as_deref(),
                Some(name.as_str()),
                "{source:?}: {html}"
            );
            rest = &after[name.len()..];
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let end = rest.find('>').expect("a tag ends");
        let attribute_text = &rest[name.len()..end];
        // Only code blocks hold the highlighter's spans, and each has one of its token colors.
        let highlight = name == "span"
            && open.ends_with(&["pre".to_owned(), "code".to_owned()])
            && HIGHLIGHT_ATTRIBUTES.contains(&attribute_text);
        assert!(
            highlight || ALLOWED_ELEMENTS.contains(&name.as_str()),
            "<{name}> from {source:?}: {html}"
        );
        let mut attrs = rest[name.len()..end].trim_end_matches('/');
        while let Some(start) = attrs.find(|c: char| !c.is_whitespace()) {
            attrs = &attrs[start..];
            let name_end = attrs.find(['=', ' ']).unwrap_or(attrs.len());
            let attr = attrs[..name_end].to_ascii_lowercase();
            assert!(!attr.starts_with("on"), "{attr} from {source:?}: {html}");
            let value = match attrs[name_end..].strip_prefix('=') {
                Some(quoted) => {
                    let inner = quoted.strip_prefix('"').expect("values are quoted");
                    let close = inner.find('"').expect("values end");
                    attrs = &inner[close + 1..];
                    &inner[..close]
                }
                None => {
                    attrs = &attrs[name_end..];
                    ""
                }
            };
            match attr.as_str() {
                "id" => assert!(
                    value.starts_with("user-content-"),
                    "{value} from {source:?}: {html}"
                ),
                "href" => assert_safe_url(source, value, true),
                "src" => assert_safe_url(source, value, false),
                // Each candidate of a `srcset` is an address, then an optional descriptor.
                "srcset" => {
                    for candidate in value.split(',') {
                        let url = candidate.split_ascii_whitespace().next().unwrap_or("");
                        assert_safe_url(source, url, false);
                    }
                }
                _ => {}
            }
        }
        if !VOID.contains(&name.as_str()) {
            open.push(name);
        }
        rest = &rest[end + 1..];
    }
    assert!(open.is_empty(), "unclosed {open:?} from {source:?}: {html}");
}

fn assert_safe_url(source: &str, value: &str, mail: bool) {
    let lower = value.to_ascii_lowercase();
    let relative = !value.starts_with("//")
        && !value
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .contains(':');
    let ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || (mail && lower.starts_with("mailto:"))
        || relative;
    assert!(ok, "unsafe address {value:?} from {source:?}");
}

/// Hostile sources. Each must render to safe markup in both modes.
const CORPUS: &[&str] = &[
        "<img src=x onerror=alert(1)>",
        "<img src=\"x\" onerror=\"y\">",
        "<img src=javascript:alert(1)>",
        "<img src=\"javascript:alert(1)\" alt=\"x\">",
        "<img src=\"//evil.test/x.png\">",
        "<img src=\"data:image/svg+xml,<svg onload=x>\">",
        "<img alt=\"x\" onerror=\"y\" src=\"z.png\">",
        "<img alt='\"><script>' src=\"x.png\">",
        "<img src=x/onerror=alert(1)>",
        "<img src=`x` alt=`y`>",
        "<img src=\"a.png\" src=\"javascript:x\">",
        "<IMG SRC=x ONERROR=alert(1)>",
        "<a href=\"&#106;avascript:alert(1)\">x</a>",
        "<a href=\"&#x6A;avascript:alert(1)\">x</a>",
        "<a href=\"&#106avascript:alert(1)\">x</a>",
        "<a href=\"javascript&colon;alert(1)\">x</a>",
        "<a href=\"java&Tab;script:alert(1)\">x</a>",
        "<a href=\"java&NewLine;script:alert(1)\">x</a>",
        "<a href=\"&amp;#106;avascript:alert(1)\">x</a>",
        "<a href=\"JaVaScRiPt:alert(1)\">x</a>",
        "<a href=\"  javascript:alert(1)\">x</a>",
        "<a href=\"\tjavascript:alert(1)\">x</a>",
        "<a href=\"javascript:alert(1)\">x</a>",
        "<a href=\"data:text/html,x\">x</a>",
        "<a href=\"//evil.test/x\">x</a>",
        "<a href=\"http:/x\">x</a>",
        "<a href=\"a\\b\">x</a>",
        "<a href=\"x&unknown;y\">x</a>",
        "<a href=\"x\0y\">x</a>",
        "<A HREF=\"javascript:alert(1)\">x</A>",
        "<a href=\"https://x.test\" target=\"_self\" rel=\"opener\" ping=\"https://evil.test\">x</a>",
        "<a title=\"x\" title=\"y\" href=\"https://x.test\">z</a>",
        "<a\0name=\"x\">",
        "<a name=x></a><a id=\"repo-nav\"></a>",
        "<a id=\"top\"></a>\n\n# top",
        "<picture><source srcset=\"ok.png 1x, javascript:x 2x\"><img src=\"ok.png\"></picture>",
        "<picture><source srcset=\"javascript:x\"><img src=\"ok.png\" alt=\"x\"></picture>",
        "<picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"//evil.test/x.png\"></picture>",
        "<svg><script>alert(1)</script></svg>",
        "<svg onload=\"alert(1)\"></svg>",
        "<math><mi xlink:href=\"javascript:alert(1)\">x</mi></math>",
        "<math><mtext><style>x</style></mtext></math>",
        "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\">x</p></noscript>",
        "<template><img src=x onerror=alert(1)></template>",
        "<style>body{}</style>",
        "<base href=\"https://evil.test/\">",
        "<meta http-equiv=\"refresh\" content=\"0;url=javascript:alert(1)\">",
        "<link rel=\"stylesheet\" href=\"https://evil.test/x.css\">",
        "<form action=\"https://evil.test\"><button>go</button></form>",
        "<iframe src=\"https://evil.test\"></iframe>",
        "<object data=\"x\"></object>",
        "<embed src=\"x\">",
        "<script>alert(1)</script>",
        "<details><details><summary>x",
        "<div>\n</div></details></table>",
        "<a><a href=\"https://x.test\">y</a></a>",
        "<li>x</li>\n<tr><td>x</td></tr>",
        "<b>a\n\nb</b>",
        "- <b>a\n\n  b</b>",
        "> <b>a\n>\n> b</b>",
        "<div>\n\n# <script>\n\n</div>",
        "<details open ontoggle=alert(1)><summary>x</summary>y</details>",
        "<DETAILS OPEN onToggle=alert(1)><SUMMARY>x</SUMMARY></DETAILS>",
        "<!--> <img src=x onerror=alert(1)>",
        "<!-- --!> <img src=x onerror=alert(1)>",
        "<![CDATA[<img src=x onerror=alert(1)>]]>",
        "<!-- x -- <img src=x onerror=alert(1)>",
        "<p style=\"background:url(javascript:x)\" class=\"x\" data-x=\"y\" onclick=x>z</p>",
        "<h1 align=\"left\" onclick=\"x\">t</h1>",
        "<td colspan=\"9999999\" rowspan=\"-1\">x</td>",
        "<ol start=\"x\"><li>a</li></ol>",
        "<kbd>&lt;script&gt;</kbd> <sub>&#60;img&#62;</sub>",
        "[<script>](https://example.org/\"onmouseover=\"bad)",
        "[x](javascript:alert(1))  <b>",
        "text <b",
        "text <!--",
        "<a x=\"",
        "<a ",
        "<a x='y'",
        "&#",
        "&#x",
        "&amp",
    ];

#[test]
fn adversarial_html_renders_only_safe_markup() {
    for &source in CORPUS {
        for html in [parse_markdown(source), parse_document(source)] {
            assert_safe_html(source, &html);
        }
    }
}

#[test]
fn open_elements_stop_at_the_depth_limit() {
    let source = "<div>".repeat(100);
    let html = parse_markdown(&source);
    assert_eq!(html.matches("<div>").count(), MAX_OPEN_ELEMENTS);
    assert_eq!(html.matches("</div>").count(), MAX_OPEN_ELEMENTS);
    assert_safe_html(&source, &html);
}

#[test]
fn readme_header_renders_picture_links_and_badges() {
    let source = "<p align=\"center\"><picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"docs/logo-dark.png\"><img src=\"docs/logo.png\" width=\"200\" alt=\"Logo\"></picture><br><a href=\"https://ci.example/x\"><img src=\"https://img.shields.io/badge/a-b-green?style=flat&logo=x\"></a></p>";
    let expected = "<p align=\"center\"><picture><source srcset=\"docs/logo-dark.png\" media=\"(prefers-color-scheme: dark)\"><img src=\"docs/logo.png\" alt=\"Logo\" width=\"200\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" /></picture><br><a href=\"https://ci.example/x\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\"><img src=\"https://img.shields.io/badge/a-b-green?style=flat&amp;logo=x\" alt=\"\" loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" /></a></p>";
    let html = parse_markdown(source);
    assert_eq!(html, expected);
    assert_safe_html(source, &html);
    let mut base = base();
    base.directory = String::new();
    let html = base.rewrite(&parse_document(source)).unwrap();
    assert!(html.contains("src=\"/alice/demo/raw/main/docs/logo.png\""));
    assert!(html.contains("srcset=\"/alice/demo/raw/main/docs/logo-dark.png\""));
    assert!(html.contains("<a href=\"https://ci.example/x\" target=\"_blank\""));
}

#[test]
fn picture_sources_resolve_every_candidate() {
    let html = base()
            .rewrite(&parse_document(
                "<picture><source srcset=\"small.png 1x, large.png 2x\"><img src=\"fallback.png\"></picture>",
            ))
            .unwrap();
    assert!(html.contains(
            "<source srcset=\"/alice/demo/raw/main/docs/small.png 1x, /alice/demo/raw/main/docs/large.png 2x\">"
        ));
    assert!(html.contains("<img src=\"/alice/demo/raw/main/docs/fallback.png\""));
    // One unsafe candidate drops the whole attribute, so the source goes and the image shows.
    let html = base()
            .rewrite(&parse_document(
                "<picture><source srcset=\"small.png 1x, javascript:x 2x\"><img src=\"fallback.png\"></picture>",
            ))
            .unwrap();
    assert!(!html.contains("srcset") && !html.contains("javascript"));
    assert!(html.contains("<img src=\"/alice/demo/raw/main/docs/fallback.png\""));
}

#[test]
fn details_hold_markdown_between_their_tags() {
    let source = "<details><summary>Click</summary>\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\n</details>";
    let html = parse_markdown(source);
    assert!(
        html.starts_with("<details><summary>Click</summary>\n<table>"),
        "{html}"
    );
    assert!(html.contains("<pre class="), "{html}");
    assert!(html.ends_with("</details>"), "{html}");
    assert_safe_html(source, &html);
}

#[test]
fn html_tables_keep_spans_and_alignment() {
    assert_eq!(
        parse_markdown(
            "<table>\n<tr><th align=\"left\">Name</th><td colspan=\"2\">x</td></tr>\n</table>"
        ),
        "<table>\n<tr><th align=\"left\">Name</th><td colspan=\"2\">x</td></tr>\n</table>"
    );
}

#[test]
fn named_anchors_are_targets_for_in_page_links() {
    assert_eq!(
            parse_markdown("<a name=\"top\"></a>\n\n[up](#top)"),
            "<p class=\"my-3 leading-7 text-foreground\"><a id=\"user-content-top\"></a></p>\n<p class=\"my-3 leading-7 text-foreground\"><a href=\"#user-content-top\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-primary hover:underline\">up</a></p>"
        );
}

#[test]
fn raw_ids_with_the_prefix_keep_one_prefix_for_in_page_links() {
    let html = parse_markdown("<a id=\"user-content-top\"></a>\n\n[up](#user-content-top)");
    assert!(html.contains("<a id=\"user-content-top\"></a>"), "{html}");
    assert!(
        html.contains("<a href=\"#user-content-top\" target=\"_blank\""),
        "{html}"
    );
    assert!(!html.contains("user-content-user-content-"), "{html}");
}

#[test]
fn keyboard_subscript_and_entities_in_raw_html() {
    assert_eq!(
        parse_inline("Press <kbd>Ctrl</kbd>, H<sub>2</sub>O"),
        "Press <kbd>Ctrl</kbd>, H<sub>2</sub>O"
    );
    assert_eq!(
        parse_markdown("<div>a &amp; &eacute; b</div>"),
        "<div>a &amp; &amp;eacute; b</div>"
    );
}

#[test]
fn unclosed_elements_end_with_their_markdown_container() {
    assert_eq!(
            parse_markdown("<b>a\n\nb</b>"),
            "<p class=\"my-3 leading-7 text-foreground\"><b>a</b></p>\n<p class=\"my-3 leading-7 text-foreground\">b&lt;/b&gt;</p>"
        );
    assert_eq!(parse_markdown("<li>x</li>"), "&lt;li&gt;x&lt;/li&gt;");
}

#[test]
fn comments_are_dropped_and_an_open_one_is_text() {
    assert_eq!(parse_inline("a<!-- hidden -->b"), "ab");
    assert!(parse_markdown("<!-- open\n\nafter").starts_with("&lt;!-- open"));
}

#[test]
fn crlf_is_normalised() {
    let html = parse_markdown("# Title\r\n\r\nbody");
    assert!(html.contains("<h1"));
    assert!(html.contains("<p class=\"my-3 leading-7 text-foreground\">body</p>"));
    assert!(!html.contains('\r'));
}
