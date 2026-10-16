//! SQLite's built-in LIKE folds ASCII only. Application search preserves Unicode
//! case-insensitive LIKE patterns in this DB-owned matcher, over scoped ordered
//! native row streams (no unbounded result buffering or driver escape).
use anyhow::Result;
use futures::{TryStream, TryStreamExt};
use tokio::sync::Semaphore;

static MATCH_SLOTS: Semaphore = Semaphore::const_new(2);
const MATCH_BATCH_ROWS: usize = 32;

#[derive(Clone, Copy)]
enum Token {
    Literal(char),
    One,
    Many,
}

#[derive(Clone)]
pub(super) struct Pattern(Vec<Token>);
impl Pattern {
    pub(super) fn contains(query: &str) -> Self {
        let input = format!("%{query}%").to_lowercase();
        let mut chars = input.chars();
        let mut tokens = Vec::new();
        while let Some(character) = chars.next() {
            tokens.push(match character {
                '%' => Token::Many,
                '_' => Token::One,
                '\\' => Token::Literal(chars.next().unwrap_or('\\')),
                character => Token::Literal(character),
            });
        }
        Self(tokens)
    }

    pub(super) fn matches(&self, value: &str) -> bool {
        let mut previous = vec![false; self.0.len() + 1];
        let mut next = vec![false; self.0.len() + 1];
        previous[0] = true;
        for (index, token) in self.0.iter().enumerate() {
            previous[index + 1] = matches!(token, Token::Many) && previous[index];
        }
        for character in value.to_lowercase().chars() {
            next[0] = false;
            for (index, token) in self.0.iter().enumerate() {
                next[index + 1] = match token {
                    Token::Many => next[index] || previous[index + 1],
                    Token::One => previous[index],
                    Token::Literal(expected) => previous[index] && *expected == character,
                };
            }
            std::mem::swap(&mut previous, &mut next);
        }
        previous[self.0.len()]
    }

    /// Steps that `matches` spends on `value`.
    pub(super) fn cost(&self, value: &str) -> usize {
        value.len() * self.0.len()
    }
}

/// Steps one search may spend on one kind of row. A step is one text byte
/// compared with one pattern token, so a 256-character query against a
/// maximum-size issue body costs about 17 million steps. The budget allows
/// about three such bodies.
pub(super) const MATCH_BUDGET: usize = 50_000_000;

/// Returns up to `limit` rows, in stream order, whose text matches `pattern`.
/// `fields` gives the texts each row is matched on (unused slots are `None`). The scan stops before a row
/// that would exceed the budget that remains, so a broad search over a large
/// table reads a bounded number of rows.
pub(super) async fn collect_matches<T, S>(
    mut rows: S,
    pattern: &Pattern,
    limit: i64,
    fields: fn(&T) -> [Option<&str>; 3],
) -> Result<Vec<T>>
where
    T: Send + 'static,
    S: TryStream<Ok = T, Error = sqlx::Error> + Unpin,
{
    let mut budget = MATCH_BUDGET;
    let mut result = Vec::new();
    let mut done = false;
    while !done && i64::try_from(result.len())? < limit {
        let batch_limit = MATCH_BATCH_ROWS.min(usize::try_from(limit)? - result.len());
        let mut batch = Vec::new();
        while batch.len() < batch_limit {
            let Some(row) = rows.try_next().await? else {
                done = true;
                break;
            };
            let cost: usize = fields(&row)
                .into_iter()
                .flatten()
                .map(|text| pattern.cost(text))
                .sum();
            if cost > budget {
                done = true;
                break;
            }
            budget -= cost;
            batch.push(row);
        }
        // Take the slot only for matching. A slot holder must not wait for the database,
        // because a search waiting for a slot may hold the connection that holder needs.
        let permit = MATCH_SLOTS.acquire().await?;
        let batch_pattern = pattern.clone();
        let matched = tokio::task::spawn_blocking(move || {
            // A cancelled search keeps its slot until this batch has finished.
            let _permit = permit;
            batch.retain(|row| {
                fields(row)
                    .into_iter()
                    .flatten()
                    .any(|text| batch_pattern.matches(text))
            });
            batch
        })
        .await?;
        result.extend(matched);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::Pattern;
    #[test]
    fn unicode_case_wildcards_and_escapes_are_not_ascii_like() {
        assert!(Pattern::contains("café").matches("A CAFÉ repository"));
        assert!(Pattern::contains("f_o%").matches("FOO bar"));
        assert!(Pattern::contains(r"a\%b").matches("literal A%B"));
        assert!(!Pattern::contains(r"a\%b").matches("arbitrary ab"));
        assert!(Pattern::contains("").matches(""));
        assert!(!Pattern::contains("different").matches("CAFÉ"));
    }
}
