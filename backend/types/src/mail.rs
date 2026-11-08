use crate::http_config::public_origin;
/// The offer of a seat in organization `name`. The database queues it in the transaction that
/// saves the invitation, linked to it so exhausted SMTP retries can be reported.
pub fn organization_invitation(name: &str) -> crate::mail_messages::Mail {
    crate::mail_messages::organization_invitation(&public_origin(), name)
}

/// Tells the person who asked for an account that an administrator decided on the request. The
/// database queues it in the transaction that records the decision.
pub fn signup_decision(username: &str, approved: bool) -> crate::mail_messages::Mail {
    crate::mail_messages::signup_decision(&public_origin(), username, approved)
}
