use super::{json, query, Value};
use axum::Router;
pub(super) async fn exercise(
    router: &Router,
    token: &str,
    repository: &str,
    issue: &str,
    pull: &str,
    user: &str,
) {
    let label=query(router,token,"mutation($input:CreateLabelInput!){createLabel(input:$input){clientMutationId label{id name color}}}",json!({"input":{"repositoryId":repository,"name":"cli-created","color":"006600","clientMutationId":"label"}})).await;
    assert_eq!(label["createLabel"]["clientMutationId"], "label");
    assert_eq!(label["createLabel"]["label"]["color"], "006600");
    let label = label["createLabel"]["label"]["id"].clone();
    query(
        router,
        token,
        "mutation($input:UpdateLabelInput!){updateLabel(input:$input){label{id name}}}",
        json!({"input":{"id":label,"name":"cli-updated"}}),
    )
    .await;
    let attached=query(router,token,"mutation($input:AddLabelsToLabelableInput!){addLabelsToLabelable(input:$input){labelable{labels(first:10){totalCount nodes{id name}}}}}",json!({"input":{"labelableId":issue,"labelIds":[label]}})).await;
    assert_eq!(
        attached["addLabelsToLabelable"]["labelable"]["labels"]["totalCount"],
        1
    );
    query(router,token,"mutation($input:RemoveLabelsFromLabelableInput!){removeLabelsFromLabelable(input:$input){labelable{labels(first:10){totalCount}}}}",json!({"input":{"labelableId":issue,"labelIds":[label]}})).await;
    let updated=query(router,token,"mutation($input:UpdatePullRequestInput!){updatePullRequest(input:$input){pullRequest{labels(first:10){totalCount}}}}",json!({"input":{"pullRequestId":pull,"labelIds":[label]}})).await;
    assert_eq!(
        updated["updatePullRequest"]["pullRequest"]["labels"]["totalCount"],
        1
    );
    for subject in [issue, pull] {
        let removed=query(router,token,"mutation($input:RemoveAssigneesFromAssignableInput!){removeAssigneesFromAssignable(input:$input){assignable{assignees(first:10){totalCount}}}}",json!({"input":{"assignableId":subject,"assigneeIds":[user]}})).await;
        assert_eq!(
            removed["removeAssigneesFromAssignable"]["assignable"]["assignees"]["totalCount"],
            0
        );
        let added=query(router,token,"mutation($input:AddAssigneesToAssignableInput!){addAssigneesToAssignable(input:$input){assignable{assignees(first:10){totalCount}}}}",json!({"input":{"assignableId":subject,"assigneeIds":[user]}})).await;
        assert_eq!(
            added["addAssigneesToAssignable"]["assignable"]["assignees"]["totalCount"],
            1
        );
        let locked=query(router,token,"mutation($input:LockLockableInput!){lockLockable(input:$input){lockedRecord{locked activeLockReason}}}",json!({"input":{"lockableId":subject,"lockReason":"RESOLVED"}})).await;
        assert_eq!(locked["lockLockable"]["lockedRecord"]["locked"], true);
        assert_eq!(
            locked["lockLockable"]["lockedRecord"]["activeLockReason"],
            "RESOLVED"
        );
        let unlocked=query(router,token,"mutation($input:UnlockLockableInput!){unlockLockable(input:$input){unlockedRecord{locked activeLockReason}}}",json!({"input":{"lockableId":subject}})).await;
        assert_eq!(
            unlocked["unlockLockable"]["unlockedRecord"]["locked"],
            false
        );
        assert_eq!(
            unlocked["unlockLockable"]["unlockedRecord"]["activeLockReason"],
            Value::Null
        );
    }
    for subject in [issue, pull] {
        let created=query(router,token,"mutation($input:AddCommentInput!){addComment(input:$input){commentEdge{node{id body}}}}",json!({"input":{"subjectId":subject,"body":"Temporary CLI comment"}})).await;
        let id = created["addComment"]["commentEdge"]["node"]["id"].clone();
        let changed=query(router,token,"mutation($input:UpdateIssueCommentInput!){updateIssueComment(input:$input){issueComment{id body}}}",json!({"input":{"id":id,"body":"Edited CLI comment"}})).await;
        assert_eq!(
            changed["updateIssueComment"]["issueComment"]["body"],
            "Edited CLI comment"
        );
        let node = query(
            router,
            token,
            "query($id:ID!){node(id:$id){id ... on IssueComment{body}}}",
            json!({"id":id}),
        )
        .await;
        assert_eq!(node["node"]["id"], id);
        assert_eq!(node["node"]["body"], "Edited CLI comment");
        for target in [json!(subject), id.clone()] {
            let reaction=query(router,token,"mutation($input:AddReactionInput!){addReaction(input:$input){reaction{id content reactable{...on Node{id}}} reactionGroups{content viewerHasReacted}}}",json!({"input":{"subjectId":target,"content":"EYES"}})).await;
            let reaction_id = reaction["addReaction"]["reaction"]["id"]
                .as_str()
                .expect("reaction id");
            assert!(reaction_id.starts_with("REA_"));
            let node = query(
                router,
                token,
                "query($id:ID!){node(id:$id){id ...on Reaction{content}}}",
                json!({"id":reaction_id}),
            )
            .await;
            assert_eq!(node["node"]["content"], "EYES");
            let removed = query(
                router,
                token,
                "mutation($input:RemoveReactionInput!){removeReaction(input:$input){reaction{id}}}",
                json!({"input":{"subjectId":target,"content":"EYES"}}),
            )
            .await;
            assert_eq!(removed["removeReaction"]["reaction"]["id"], reaction_id);
        }
        query(router,token,"mutation($input:DeleteIssueCommentInput!){deleteIssueComment(input:$input){clientMutationId}}",json!({"input":{"id":id}})).await;
    }
    query(
        router,
        token,
        "mutation($input:DeleteLabelInput!){deleteLabel(input:$input){clientMutationId}}",
        json!({"input":{"id":label}}),
    )
    .await;
}
