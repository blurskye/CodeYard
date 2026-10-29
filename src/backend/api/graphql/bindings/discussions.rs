use super::*;

enums!(DiscussionStateReason);
objects!(
    Discussion,
    DiscussionCategory,
    DiscussionComment,
    DiscussionConnection,
    DiscussionEdge,
    DiscussionCategoryConnection,
    DiscussionCategoryEdge,
    DiscussionCommentConnection,
    DiscussionCommentEdge,
    CreateDiscussionPayload,
    UpdateDiscussionPayload,
    DeleteDiscussionPayload,
    AddDiscussionCommentPayload,
    UpdateDiscussionCommentPayload,
    DeleteDiscussionCommentPayload,
    MarkDiscussionCommentAsAnswerPayload,
    UnmarkDiscussionCommentAsAnswerPayload
);

macro_rules! mutation_fields {
    () => { fields!("Mutation";
        create_discussion(input: gql::Input<gql::CreateDiscussionInput>) -> Option<gql::CreateDiscussionPayload>;
        update_discussion(input: gql::Input<gql::UpdateDiscussionInput>) -> Option<gql::UpdateDiscussionPayload>;
        delete_discussion(input: gql::Input<gql::DeleteDiscussionInput>) -> Option<gql::DeleteDiscussionPayload>;
        add_discussion_comment(input: gql::Input<gql::AddDiscussionCommentInput>) -> Option<gql::AddDiscussionCommentPayload>;
        update_discussion_comment(input: gql::Input<gql::UpdateDiscussionCommentInput>) -> Option<gql::UpdateDiscussionCommentPayload>;
        delete_discussion_comment(input: gql::Input<gql::DeleteDiscussionCommentInput>) -> Option<gql::DeleteDiscussionCommentPayload>;
        mark_discussion_comment_as_answer(input: gql::Input<gql::MarkDiscussionCommentAsAnswerInput>) -> Option<gql::MarkDiscussionCommentAsAnswerPayload>;
        unmark_discussion_comment_as_answer(input: gql::Input<gql::UnmarkDiscussionCommentAsAnswerInput>) -> Option<gql::UnmarkDiscussionCommentAsAnswerPayload>;
    ); };
}
pub(super) use mutation_fields;
macro_rules! repository_fields {
    () => { fields!("Repository";
        has_discussions_enabled() -> bool;
        discussion(number: i32) -> Option<gql::Discussion>;
        discussion_categories(after: Option<String>, before: Option<String>, filter_by_assignable: Option<bool>, first: Option<i32>, last: Option<i32>) -> gql::DiscussionCategoryConnection;
        discussions(after: Option<String>, answered: Option<bool>, before: Option<String>, category_id: Option<ID>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::DiscussionOrder>>, states: Option<Vec<gql::DiscussionState>>) -> gql::DiscussionConnection;
    ); };
}
pub(super) use repository_fields;
impl gql::DiscussionResolver for Object {
    fields!("Discussion";
        id() -> ID; database_id() -> Option<i32>; number() -> i32; title() -> String; body() -> String;
        url() -> gql::URI; closed() -> bool; closed_at() -> Option<gql::DateTime>; state_reason() -> Option<gql::DiscussionStateReason>;
        author() -> Option<gql::Actor>; category() -> gql::DiscussionCategory; repository() -> gql::Repository;
        locked() -> bool; is_answered() -> Option<bool>; answer() -> Option<gql::DiscussionComment>;
        answer_chosen_at() -> Option<gql::DateTime>; answer_chosen_by() -> Option<gql::Actor>;
        created_at() -> gql::DateTime; updated_at() -> gql::DateTime;
        reaction_groups() -> Option<Vec<gql::ReactionGroup>>;
        labels(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>, order_by: Option<gql::Input<gql::LabelOrder>>) -> Option<gql::LabelConnection>;
        comments(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::DiscussionCommentConnection;
    );
}
impl gql::DiscussionCategoryResolver for Object {
    fields!("DiscussionCategory";
        id() -> ID; name() -> String; slug() -> String; emoji() -> String; description() -> Option<String>;
        is_answerable() -> bool; created_at() -> gql::DateTime; repository() -> gql::Repository;
    );
}
impl gql::DiscussionCommentResolver for Object {
    fields!("DiscussionComment";
        id() -> ID; database_id() -> Option<i32>; url() -> gql::URI; body() -> String;
        author() -> Option<gql::Actor>; created_at() -> gql::DateTime; updated_at() -> gql::DateTime;
        is_answer() -> bool; upvote_count() -> i32; reaction_groups() -> Option<Vec<gql::ReactionGroup>>;
        discussion() -> Option<gql::Discussion>;
        replies(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::DiscussionCommentConnection;
    );
}
connection!(
    DiscussionConnectionResolver,
    DiscussionEdgeResolver,
    "DiscussionConnection",
    "DiscussionEdge",
    Discussion,
    DiscussionEdge
);
connection!(
    DiscussionCategoryConnectionResolver,
    DiscussionCategoryEdgeResolver,
    "DiscussionCategoryConnection",
    "DiscussionCategoryEdge",
    DiscussionCategory,
    DiscussionCategoryEdge
);
connection!(
    DiscussionCommentConnectionResolver,
    DiscussionCommentEdgeResolver,
    "DiscussionCommentConnection",
    "DiscussionCommentEdge",
    DiscussionComment,
    DiscussionCommentEdge
);
macro_rules! discussion_payload {
    ($resolver:ident,$ty:literal) => { impl gql::$resolver for Object {
        fields!($ty; client_mutation_id() -> Option<String>; discussion() -> Option<gql::Discussion>;);
    } };
}
discussion_payload!(CreateDiscussionPayloadResolver, "CreateDiscussionPayload");
discussion_payload!(UpdateDiscussionPayloadResolver, "UpdateDiscussionPayload");
discussion_payload!(DeleteDiscussionPayloadResolver, "DeleteDiscussionPayload");
discussion_payload!(
    MarkDiscussionCommentAsAnswerPayloadResolver,
    "MarkDiscussionCommentAsAnswerPayload"
);
discussion_payload!(
    UnmarkDiscussionCommentAsAnswerPayloadResolver,
    "UnmarkDiscussionCommentAsAnswerPayload"
);
macro_rules! comment_payload {
    ($resolver:ident,$ty:literal) => { impl gql::$resolver for Object {
        fields!($ty; client_mutation_id() -> Option<String>; comment() -> Option<gql::DiscussionComment>;);
    } };
}
comment_payload!(
    AddDiscussionCommentPayloadResolver,
    "AddDiscussionCommentPayload"
);
comment_payload!(
    UpdateDiscussionCommentPayloadResolver,
    "UpdateDiscussionCommentPayload"
);
comment_payload!(
    DeleteDiscussionCommentPayloadResolver,
    "DeleteDiscussionCommentPayload"
);
