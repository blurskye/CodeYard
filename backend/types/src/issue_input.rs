/// Text limits for issues, pull requests, comments, reviews and labels. The JSON API and
/// the native forms both call these, so they refuse the same text. NUL is refused because
/// PostgreSQL text cannot store it, and a SQLite database holding it could not be copied.
pub fn validate_title(title: &str) -> Result<(), &'static str> {
    if title.trim().is_empty() || title.len() > 255 {
        Err("Title must contain 1–255 bytes of text.")
    } else if title.contains('\0') {
        Err(NUL_REFUSAL)
    } else {
        Ok(())
    }
}

pub fn validate_body(body: &str, required: bool) -> Result<(), &'static str> {
    if required && body.trim().is_empty() {
        return Err("Comment cannot be empty.");
    }
    if body.len() > 65_536 {
        return Err("Text cannot exceed 65,536 bytes.");
    }
    if body.contains('\0') {
        return Err(NUL_REFUSAL);
    }
    Ok(())
}

/// Label names are 1–50 bytes and descriptions at most 1,024 bytes. Both refuse NUL.
pub fn validate_label(name: &str, description: Option<&str>) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 50 {
        Err("Label name must be 1-50 characters")
    } else if description.is_some_and(|text| text.len() > 1_024) {
        Err("Label description must be at most 1024 bytes")
    } else if name.contains('\0') || description.is_some_and(|text| text.contains('\0')) {
        Err(NUL_REFUSAL)
    } else {
        Ok(())
    }
}

const NUL_REFUSAL: &str = "Text cannot contain NUL characters.";

/// Milestone titles are 1–255 bytes. The description has the body limits.
pub fn validate_milestone(title: &str, description: &str) -> Result<(), &'static str> {
    if title.trim().is_empty() || title.len() > 255 {
        Err("Milestone title must contain 1–255 bytes of text.")
    } else if title.contains('\0') {
        Err(NUL_REFUSAL)
    } else {
        validate_body(description, false)
    }
}

/// A calendar day written exactly YYYY-MM-DD; other spellings of the day, such as 2026-1-2,
/// are refused.
pub fn calendar_day(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .filter(|day| day.format("%Y-%m-%d").to_string() == value)
}

/// A due date is a calendar day written YYYY-MM-DD. An empty value means no due date.
pub fn parse_due_date(value: &str) -> Result<Option<String>, &'static str> {
    if value.is_empty() {
        return Ok(None);
    }
    calendar_day(value)
        .map(|_| Some(value.to_owned()))
        .ok_or("Due date must be a day written YYYY-MM-DD.")
}
