#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Private security reporting denied")]
    Denied,
    #[error("Security report not found")]
    Missing,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Security reporting limit reached")]
    Limit,
}

pub struct NewReport<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

impl<'a> NewReport<'a> {
    pub fn new(title: &'a str, body: &'a str) -> std::result::Result<Self, Refusal> {
        if title.trim().is_empty() || title.len() > 255 || title.chars().any(char::is_control) {
            return Err(Refusal::Invalid(
                "A report needs a single-line title of at most 255 bytes",
            ));
        }
        validate_body(body)?;
        Ok(Self { title, body })
    }
}

pub fn validate_body(body: &str) -> std::result::Result<(), Refusal> {
    if body.trim().is_empty() || body.len() > 65536 || body.contains('\0') {
        return Err(Refusal::Invalid(
            "A report or reply needs text of at most 65536 bytes",
        ));
    }
    Ok(())
}
