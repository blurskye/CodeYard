//! Picture forms: a settings form that sends a picture as `multipart/form-data`,
//! a few small text fields (the CSRF token) and one file named `picture`.
use crate::backend::identity::avatars::MAX_PICTURE_BYTES;
use axum::{extract::Multipart, http::StatusCode};
use std::collections::HashMap;
use std::time::Duration;

/// The most a request may carry: the largest picture plus room for the form fields.
pub const BODY_LIMIT: usize = MAX_PICTURE_BYTES + 16 * 1024;
const MAX_FIELDS: usize = 8;
const MAX_FIELD_BYTES: usize = 256;
/// How long a picture form may take to arrive. A sender that stalls holds a picture slot
/// until then.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub struct PictureForm {
    /// The text fields.
    pub fields: HashMap<String, String>,
    /// The bytes of the `picture` file, if one was sent.
    pub picture: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PictureFormError {
    TooLarge,
    Unreadable,
    TimedOut,
}

impl PictureFormError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unreadable => StatusCode::BAD_REQUEST,
            Self::TimedOut => StatusCode::REQUEST_TIMEOUT,
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::TooLarge => "The picture is larger than 2 MB.",
            Self::Unreadable => "The form could not be read. Choose the picture again.",
            Self::TimedOut => "The form took too long to arrive. Choose the picture again.",
        }
    }
}

fn read_error(error: axum::extract::multipart::MultipartError) -> PictureFormError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        PictureFormError::TooLarge
    } else {
        PictureFormError::Unreadable
    }
}

/// Reads the whole form, stopping as soon as a part is larger than allowed or the form
/// is not complete within `READ_TIMEOUT`.
pub async fn read(multipart: Multipart) -> Result<PictureForm, PictureFormError> {
    tokio::time::timeout(READ_TIMEOUT, read_parts(multipart))
        .await
        .unwrap_or(Err(PictureFormError::TimedOut))
}

async fn read_parts(mut multipart: Multipart) -> Result<PictureForm, PictureFormError> {
    let mut form = PictureForm {
        fields: HashMap::new(),
        picture: None,
    };
    let mut parts = 0;
    while let Some(mut field) = multipart.next_field().await.map_err(read_error)? {
        parts += 1;
        if parts > MAX_FIELDS {
            return Err(PictureFormError::Unreadable);
        }
        let name = field.name().unwrap_or_default().to_owned();
        let is_picture = name == "picture";
        let limit = if is_picture {
            MAX_PICTURE_BYTES
        } else {
            MAX_FIELD_BYTES
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(read_error)? {
            if bytes.len() + chunk.len() > limit {
                return Err(if is_picture {
                    PictureFormError::TooLarge
                } else {
                    PictureFormError::Unreadable
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if is_picture {
            if form.picture.replace(bytes).is_some() {
                return Err(PictureFormError::Unreadable);
            }
        } else {
            let value = String::from_utf8(bytes).map_err(|_| PictureFormError::Unreadable)?;
            form.fields.insert(name, value);
        }
    }
    Ok(form)
}
