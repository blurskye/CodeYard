//! Task list items (`- [ ]` and `- [x]`) in a stored body. The lists count them, and a box
//! ticks one item of the stored text. Items are numbered in source order, the same numbers the
//! Markdown renderer gives the boxes it writes.

use super::markdown::markdown_options;
use pulldown_cmark::{Event, Parser};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskCount {
    pub done: usize,
    pub total: usize,
}

/// How many task list items the text has, and how many are ticked.
pub fn count_tasks(text: &str) -> TaskCount {
    let mut count = TaskCount::default();
    for event in Parser::new_ext(text, markdown_options(false)) {
        if let Event::TaskListMarker(checked) = event {
            count.total += 1;
            count.done += usize::from(checked);
        }
    }
    count
}

/// The text with item `index` ticked or unticked, and the rest of it unchanged. `None` when the
/// text has no such item.
pub fn toggle_task(text: &str, index: usize) -> Option<String> {
    let (checked, range) = Parser::new_ext(text, markdown_options(false))
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::TaskListMarker(checked) => Some((checked, range)),
            _ => None,
        })
        .nth(index)?;
    let mark = if checked { "[ ]" } else { "[x]" };
    Some(format!(
        "{}{mark}{}",
        &text[..range.start],
        &text[range.end..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_ticked_and_total_items() {
        assert_eq!(
            count_tasks("- [ ] open\n- [x] done\n- [X] also done\n- plain"),
            TaskCount { done: 2, total: 3 }
        );
        assert_eq!(count_tasks("no list here"), TaskCount::default());
    }

    #[test]
    fn items_inside_code_are_not_counted() {
        assert_eq!(
            count_tasks("```\n- [ ] not a task\n```\n"),
            TaskCount::default()
        );
    }

    #[test]
    fn toggling_rewrites_only_the_numbered_box() {
        let text = "Intro\n\n- [ ] first\n- [x] second\n  - [ ] nested\n";
        assert_eq!(
            toggle_task(text, 0).as_deref(),
            Some("Intro\n\n- [x] first\n- [x] second\n  - [ ] nested\n")
        );
        assert_eq!(
            toggle_task(text, 1).as_deref(),
            Some("Intro\n\n- [ ] first\n- [ ] second\n  - [ ] nested\n")
        );
        assert_eq!(
            toggle_task(text, 2).as_deref(),
            Some("Intro\n\n- [ ] first\n- [x] second\n  - [x] nested\n")
        );
        assert_eq!(toggle_task(text, 3), None);
    }

    #[test]
    fn a_code_sample_before_the_list_does_not_shift_the_numbers() {
        let text = "```\n- [ ] sample\n```\n\n- [ ] real\n";
        assert_eq!(
            toggle_task(text, 0).as_deref(),
            Some("```\n- [ ] sample\n```\n\n- [x] real\n")
        );
    }
}
