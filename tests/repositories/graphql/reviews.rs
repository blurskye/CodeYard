use super::{json, query};
use axum::Router;
pub(super) async fn exercise(router: &Router, token: &str, pull: &str) {
    let created=query(router,token,"mutation($input:AddPullRequestReviewInput!){addPullRequestReview(input:$input){pullRequestReview{id body state author{login}}}}",json!({"input":{"pullRequestId":pull,"body":"Draft review"}})).await;
    assert_eq!(
        created["addPullRequestReview"]["pullRequestReview"]["state"],
        "PENDING"
    );
    let id = created["addPullRequestReview"]["pullRequestReview"]["id"].clone();
    let read=query(router,token,"query($id:ID!){node(id:$id){...on PullRequestReview{id body state pullRequest{reviews(first:10){totalCount nodes{id}} latestReviews(first:10){totalCount}}}}}",json!({"id":id})).await;
    assert_eq!(read["node"]["id"], id);
    assert_eq!(read["node"]["pullRequest"]["reviews"]["totalCount"], 1);
    assert_eq!(
        read["node"]["pullRequest"]["latestReviews"]["totalCount"],
        0
    );
    let edited=query(router,token,"mutation($input:UpdatePullRequestReviewInput!){updatePullRequestReview(input:$input){pullRequestReview{id body}}}",json!({"input":{"pullRequestReviewId":id,"body":"Edited draft"}})).await;
    assert_eq!(
        edited["updatePullRequestReview"]["pullRequestReview"]["body"],
        "Edited draft"
    );
    let deleted=query(router,token,"mutation($input:DeletePullRequestReviewInput!){deletePullRequestReview(input:$input){pullRequestReview{id state}}}",json!({"input":{"pullRequestReviewId":id}})).await;
    assert_eq!(
        deleted["deletePullRequestReview"]["pullRequestReview"]["id"],
        id
    );
}
