use anyhow::Result;
pub const MAX_WORKFLOW_MATCH_WORK: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("workflow filters exceed the match-work limit")]
pub struct WorkflowMatchLimit;

/// One deterministic CPU budget shared by every workflow considered for an event.
pub struct WorkflowMatchBudget {
    remaining: u64,
}

impl WorkflowMatchBudget {
    pub fn new() -> Self {
        Self {
            remaining: MAX_WORKFLOW_MATCH_WORK,
        }
    }

    pub fn charge(
        &mut self,
        pattern_bytes: usize,
        value_bytes: usize,
    ) -> Result<(), WorkflowMatchLimit> {
        let work = (pattern_bytes as u64)
            .checked_add(1)
            .and_then(|pattern| {
                (value_bytes as u64)
                    .checked_add(1)
                    .and_then(|value| pattern.checked_mul(value))
            })
            .ok_or(WorkflowMatchLimit)?;
        if work > self.remaining {
            self.remaining = 0;
            return Err(WorkflowMatchLimit);
        }
        self.remaining -= work;
        Ok(())
    }
}

impl Default for WorkflowMatchBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// GitHub's filter glob: `*` and `?` stop at `/`, `**` crosses it, and `**/` also matches
/// no directory, so `**/README.md` matches a root README.md. Branch protection patterns use
/// this too.
pub fn github_glob_matches(pattern: &str, value: &str) -> bool {
    glob_matches(pattern, value, false, false)
}

fn glob_matches(pattern: &str, value: &str, escapes: bool, workflow: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    // Each row records which value prefixes match the pattern consumed so far.
    // O(pattern.len() * value.len()) work avoids exponential wildcard backtracking.
    let mut previous = vec![false; value.len() + 1];
    let mut current = vec![false; value.len() + 1];
    previous[0] = true;
    let mut index = 0;
    let mut next_class_end = pattern.iter().position(|&ch| ch == ']');
    while index < pattern.len() {
        current.fill(false);
        let escaped = escapes && pattern[index] == '\\' && index + 1 < pattern.len();
        if escaped {
            index += 1;
        }
        let ch = pattern[index];
        let double = !escaped && ch == '*' && pattern.get(index + 1) == Some(&'*');
        if double
            && pattern.get(index + 2) == Some(&'/')
            && (index == 0 || pattern[index - 1] == '/')
        {
            // `**/` crosses any directories and then needs a `/`, as in `a/**/b`. It can also
            // match no directory at all, so the rows from before it carry over.
            let mut reached = false;
            for (offset, &letter) in value.iter().enumerate() {
                reached |= previous[offset];
                current[offset + 1] = reached && letter == '/';
            }
            for (offset, &matched) in previous.iter().enumerate() {
                current[offset] |= matched;
            }
            index += 3;
        } else if ch == '*' && !escaped {
            current[0] = previous[0];
            for (offset, &letter) in value.iter().enumerate() {
                current[offset + 1] =
                    previous[offset + 1] || (current[offset] && (double || letter != '/'));
            }
            index += if double { 2 } else { 1 };
        } else {
            let class_end = if ch == '[' && !escaped {
                // Advance the closing-bracket search only forwards, including for malformed
                // classes, so repeated `[` characters cannot make parsing quadratic.
                if next_class_end.is_some_and(|end| end <= index) {
                    next_class_end = pattern[index + 1..]
                        .iter()
                        .position(|&ch| ch == ']')
                        .map(|offset| index + 1 + offset);
                }
                next_class_end.filter(|&end| end > index + 1)
            } else {
                None
            };
            let end = class_end.map_or(index + 1, |end| end + 1);
            let quantifier = workflow.then(|| pattern.get(end)).flatten().copied();
            let repeated = quantifier == Some('+');
            let optional = quantifier == Some('?');
            current[0] = optional && previous[0];
            for (offset, &letter) in value.iter().enumerate() {
                let matches = if let Some(end) = class_end {
                    letter != '/' && glob_class_matches(&pattern[index + 1..end], letter)
                } else if ch == '?' && !escaped {
                    letter != '/'
                } else {
                    ch == letter
                };
                current[offset + 1] = (matches
                    && (previous[offset] || (repeated && current[offset])))
                    || (optional && previous[offset + 1]);
            }
            index = end + usize::from(repeated || optional);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[value.len()]
}

fn glob_class_matches(mut class: &[char], letter: char) -> bool {
    let negated = matches!(class.first(), Some('!' | '^'));
    if negated {
        class = &class[1..];
    }
    let mut matched = false;
    while !class.is_empty() {
        if class.len() >= 3 && class[1] == '-' {
            matched |= class[0] <= letter && letter <= class[2];
            class = &class[3..];
        } else {
            matched |= class[0] == letter;
            class = &class[1..];
        }
    }
    matched != negated
}

/// Charges `budget` for the work before matching, so one budget bounds a caller's total.
pub fn github_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(github_glob_matches(pattern, value))
}

pub fn workflow_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(glob_matches(pattern, value, true, true))
}

/// CODEOWNERS uses backslashes to quote whitespace and wildcard characters.
pub fn code_owner_glob_matches_bounded(
    pattern: &str,
    value: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    budget.charge(pattern.len(), value.len())?;
    Ok(glob_matches(pattern, value, true, false))
}

/// Whether `pattern` names `branch`. A pattern without `*`, `?` or `[` is a plain name and is only
/// compared (Git ref names cannot contain those characters). A wildcard pattern is matched within `budget`, and running out of it is an error,
/// never a non-match.
pub fn branch_pattern_matches(
    pattern: &str,
    branch: &str,
    budget: &mut WorkflowMatchBudget,
) -> std::result::Result<bool, WorkflowMatchLimit> {
    if !pattern.contains(['*', '?', '[']) {
        return Ok(pattern == branch);
    }
    github_glob_matches_bounded(pattern, branch, budget)
}
