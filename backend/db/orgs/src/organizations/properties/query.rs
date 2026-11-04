use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::queries::organizations::properties::invalid;
use codeyard_db_core::queries::organizations::properties::valid_name;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Text(String),
    Name(String),
    Owner(String),
    Repository(String, String),
    Private(bool),
    Archived(bool),
    Fork(bool),
    Language(String),
    Topic(String),
    Property(String, String),
    Any,
    Never,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Criterion {
    pub negative: bool,
    pub kind: Kind,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryQuery {
    pub groups: Vec<Vec<Criterion>>,
}
impl RepositoryQuery {
    pub fn parse(source: &str) -> Result<Self> {
        ensure!(
            source.len() <= 4096,
            invalid(
                "repository_query",
                "Repository queries must contain at most 4096 UTF-8 bytes"
            )
        );
        let mut tokens = Vec::new();
        let mut token = String::new();
        let mut quoted = false;
        let mut had_quote = false;
        let mut escaped = false;
        for character in source.chars() {
            ensure!(
                character != '\0',
                invalid("repository_query", "Invalid repository query")
            );
            if escaped {
                token.push(character);
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '"' => {
                    quoted = !quoted;
                    had_quote = true;
                }
                character if character.is_whitespace() && !quoted => {
                    if !token.is_empty() {
                        tokens.push((std::mem::take(&mut token), had_quote));
                        had_quote = false;
                    }
                }
                _ => token.push(character),
            }
        }
        ensure!(
            !quoted && !escaped,
            invalid(
                "repository_query",
                "Close quoted repository query values and escapes"
            )
        );
        if !token.is_empty() {
            tokens.push((token, had_quote));
        }
        ensure!(
            tokens.len() <= 50,
            invalid(
                "repository_query",
                "Repository queries can contain at most 50 terms"
            )
        );
        if tokens.is_empty() {
            return Ok(Self::default());
        }
        let mut groups = vec![Vec::new()];
        let mut expect_term = true;
        for (token, quoted) in tokens {
            if token == "OR" && !quoted {
                ensure!(
                    !expect_term,
                    invalid(
                        "repository_query",
                        "OR requires a repository expression on each side"
                    )
                );
                groups.push(Vec::new());
                expect_term = true;
                continue;
            }
            if token == "AND" && !quoted {
                ensure!(
                    !expect_term,
                    invalid(
                        "repository_query",
                        "AND requires a repository expression on each side"
                    )
                );
                expect_term = true;
                continue;
            }
            let (negative, token) = token
                .strip_prefix('-')
                .map_or((false, token.as_str()), |value| (true, value));
            ensure!(
                !token.is_empty(),
                invalid(
                    "repository_query",
                    "A negation requires a repository expression"
                )
            );
            let kind = if let Some((key, value)) = token.split_once(':') {
                ensure!(
                    !value.is_empty(),
                    invalid(
                        "repository_query",
                        "Repository query qualifiers require a value"
                    )
                );
                match key.to_ascii_lowercase().as_str() {
                    "name" => Kind::Name(value.into()),
                    "org" | "owner" => Kind::Owner(value.into()),
                    "user" => Kind::Never,
                    "repo" => {
                        let (owner, name) = value
                            .split_once('/')
                            .filter(|(owner, name)| {
                                !owner.is_empty() && !name.is_empty() && !name.contains('/')
                            })
                            .ok_or_else(|| invalid("repository_query", "Use repo:owner/name"))?;
                        Kind::Repository(owner.into(), name.into())
                    }
                    "is" | "visibility" => match value {
                        "private" => Kind::Private(true),
                        "public" => Kind::Private(false),
                        "internal" => Kind::Never,
                        "archived" => Kind::Archived(true),
                        "fork" => Kind::Fork(true),
                        "source" => Kind::Fork(false),
                        _ => {
                            return Err(invalid(
                                "repository_query",
                                "Unsupported repository visibility qualifier",
                            )
                            .into())
                        }
                    },
                    "archived" => Kind::Archived(boolean(value)?),
                    "fork" => match value {
                        "only" => Kind::Fork(true),
                        "false" => Kind::Fork(false),
                        "true" => Kind::Any,
                        _ => {
                            return Err(invalid(
                                "repository_query",
                                "Use fork:true, fork:false or fork:only",
                            )
                            .into())
                        }
                    },
                    "language" => Kind::Language(value.into()),
                    "topic" => Kind::Topic(value.into()),
                    key if key.starts_with("props.") => {
                        let name = &key[6..];
                        ensure!(
                            valid_name(name),
                            invalid("repository_query", "Invalid repository property query name")
                        );
                        Kind::Property(name.into(), value.into())
                    }
                    _ => {
                        return Err(invalid(
                            "repository_query",
                            "Unsupported repository query qualifier",
                        )
                        .into())
                    }
                }
            } else {
                Kind::Text(token.into())
            };
            groups
                .last_mut()
                .expect("initial group")
                .push(Criterion { negative, kind });
            expect_term = false;
        }
        ensure!(
            !expect_term,
            invalid(
                "repository_query",
                "A repository expression must follow AND or OR"
            )
        );
        Ok(Self { groups })
    }
}
fn boolean(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid(
            "repository_query",
            "Boolean repository qualifiers require true or false",
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orgs_property_repository_queries_keep_groups_and_literal_values() {
        let query = RepositoryQuery::parse(
            "archived:false props.Environment:\"production east\" OR -repo:other/private",
        )
        .unwrap();
        assert_eq!(query.groups.len(), 2);
        assert_eq!(
            query.groups[0][1].kind,
            Kind::Property("environment".into(), "production east".into())
        );
        assert!(query.groups[1][0].negative);
        for query in [
            "OR repo:one/two",
            "repo:one/two OR",
            "props.:a",
            "repo:one",
            "archived:maybe",
            "name:\"unfinished",
        ] {
            assert!(RepositoryQuery::parse(query).is_err(), "{query}");
        }
    }
}
