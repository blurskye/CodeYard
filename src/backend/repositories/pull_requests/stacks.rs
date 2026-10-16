//! Stacks: pull requests in one repository that build on each other, each one's base branch
//! being the head branch of the pull request below it. A pull request joins a stack only when
//! someone asks it to; the stack id is stored, and the order inside a stack is read from the
//! branch names.

use crate::backend::db;
use crate::backend::db::models::PullRequest;
use crate::backend::db::DbPool;
use std::collections::HashSet;

/// A stack in the order a reader sees it: merged layers first, then the open layers from the
/// bottom up.
pub struct Stack {
    pub id: i64,
    pub layers: Vec<PullRequest>,
}

impl Stack {
    /// The one-based place of the layer with this pull request id, or zero when it is not here.
    pub fn position(&self, pull_request_id: i64) -> usize {
        self.layers
            .iter()
            .position(|layer| layer.id == pull_request_id)
            .map_or(0, |index| index + 1)
    }

    /// The branch the bottom layer is based on.
    pub fn trunk(&self) -> &str {
        self.layers
            .first()
            .map_or("", |layer| layer.base_branch.as_str())
    }
}

/// Orders the members of a stack for display: merged layers first, by merge time, since merging
/// goes from the bottom up. The open layers follow along their branches, and closed layers come
/// last, in number order. Merge decisions do not read this order; see `chain_in`.
pub fn order_layers(mut members: Vec<PullRequest>) -> Vec<PullRequest> {
    members.sort_by_key(|pull| pull.number);
    let (mut merged, rest): (Vec<PullRequest>, Vec<PullRequest>) =
        members.into_iter().partition(|pull| pull.state == "merged");
    // A closed layer is not part of the open chain: its branches no longer order anything.
    let (open, closed): (Vec<PullRequest>, Vec<PullRequest>) =
        rest.into_iter().partition(|pull| pull.state == "open");
    merged.sort_by_key(|pull| (pull.merged_at, pull.number));
    merged = order_by_branches(merged);
    merged.extend(order_open_layers(open));
    merged.extend(closed);
    merged
}

/// Puts merged layers in the order their branches stack them, bottom first. Layers merged in one
/// batch share a merge time, so the branches decide between them. The order they arrive in, which
/// is merge time and then number, is kept wherever the branches do not decide.
fn order_by_branches(members: Vec<PullRequest>) -> Vec<PullRequest> {
    let mut pending = members;
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        // The next layer is one whose base is no other pending layer's head.
        let next = pending
            .iter()
            .position(|pull| {
                !pending
                    .iter()
                    .any(|other| other.id != pull.id && other.head_branch == pull.base_branch)
            })
            .unwrap_or(0);
        ordered.push(pending.remove(next));
    }
    ordered
}

/// Orders unmerged members, given in number order, from the bottom up. The bottom is the member
/// whose base is no other member's head branch; each next layer is the one based on the head of
/// the layer below. When the branches do not form one line (a cycle, or two layers on one head),
/// the members stay in number order.
fn order_open_layers(members: Vec<PullRequest>) -> Vec<PullRequest> {
    if members.is_empty() {
        return members;
    }
    let heads: HashSet<&str> = members
        .iter()
        .map(|pull| pull.head_branch.as_str())
        .collect();
    let Some(bottom) = members
        .iter()
        .position(|pull| !heads.contains(pull.base_branch.as_str()))
    else {
        return members;
    };
    let mut order = vec![bottom];
    let mut seen = HashSet::from([bottom]);
    while order.len() < db::pull_requests::records::MAX_STACK_LAYERS {
        let head = members[order[order.len() - 1]].head_branch.as_str();
        let above: Vec<usize> = (0..members.len())
            .filter(|&index| members[index].base_branch == head)
            .collect();
        match above.as_slice() {
            [next] if seen.insert(*next) => order.push(*next),
            _ => break,
        }
    }
    if order.len() != members.len() {
        return members;
    }
    let mut slots: Vec<Option<PullRequest>> = members.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

/// The stack a pull request belongs to. A stack with one layer left is not shown as a stack.
pub async fn stack_of(pool: &DbPool, pull: &PullRequest) -> anyhow::Result<Option<Stack>> {
    let Some(id) = pull.stack_id else {
        return Ok(None);
    };
    let members = db::pull_requests::records::list_stack_members(pool, pull.repo_id, id).await?;
    if members.len() < 2 {
        return Ok(None);
    }
    Ok(Some(Stack {
        id,
        layers: order_layers(members),
    }))
}

/// The member of a stack that `pull` is based on: the one whose head branch is `pull`'s base
/// branch. When several members share that head, an open one wins, then a merged one, then a
/// closed one; ties go to the lowest number.
fn predecessor<'a>(pull: &PullRequest, members: &'a [PullRequest]) -> Option<&'a PullRequest> {
    members
        .iter()
        .filter(|member| member.id != pull.id && member.head_branch == pull.base_branch)
        .min_by_key(|member| (state_rank(&member.state), member.number))
}

fn state_rank(state: &str) -> u8 {
    match state {
        "open" => 0,
        "merged" => 1,
        _ => 2,
    }
}

/// What merging a pull request brings in, read from its stack.
pub enum Chain {
    /// The open layers from the bottom of the stack up to the pull request, bottom first. The
    /// pull request is last; a pull request in no stack is the whole chain.
    Open(Vec<PullRequest>),
    /// The layer with this number below one of the layers is closed. Reopening it or unstacking
    /// the pull request clears it.
    BelowClosed(i32),
    /// The layers are based on each other in a loop. Joins refuse that, so only stored data can
    /// hold it.
    Broken,
}

/// Walks down from `pull` through the layers it is based on. The walk stops at a merged layer or
/// at the bottom. The answer does not depend on how `members` is listed.
fn chain_in(pull: &PullRequest, members: &[PullRequest]) -> Chain {
    let mut below: Vec<&PullRequest> = Vec::new();
    let mut current = pull;
    while let Some(layer) = predecessor(current, members) {
        if layer.state == "merged" {
            break;
        }
        if layer.state != "open" {
            return Chain::BelowClosed(layer.number);
        }
        if layer.id == pull.id || below.iter().any(|seen| seen.id == layer.id) {
            return Chain::Broken;
        }
        below.push(layer);
        current = layer;
    }
    let mut layers: Vec<PullRequest> = below.into_iter().rev().cloned().collect();
    layers.push(pull.clone());
    Chain::Open(layers)
}

/// The chain that merging `pull` brings in.
pub async fn chain_below(pool: &DbPool, pull: &PullRequest) -> anyhow::Result<Chain> {
    let members = match pull.stack_id {
        Some(id) => db::pull_requests::records::list_stack_members(pool, pull.repo_id, id).await?,
        None => Vec::new(),
    };
    Ok(chain_in(pull, &members))
}

/// The branch that merging `pull` lands in, and the numbers of the pull requests that merge with it,
/// bottom first and `pull` last. The branch is the base of the bottom open layer of the stack, and
/// a pull request outside a stack, or one whose stack is broken, merges into its own base alone.
pub async fn merge_target(pool: &DbPool, pull: &PullRequest) -> anyhow::Result<(String, Vec<i32>)> {
    Ok(match chain_below(pool, pull).await? {
        Chain::Open(layers) => (
            layers.first().map_or_else(
                || pull.base_branch.clone(),
                |bottom| bottom.base_branch.clone(),
            ),
            layers.iter().map(|layer| layer.number).collect(),
        ),
        Chain::BelowClosed(_) | Chain::Broken => (pull.base_branch.clone(), vec![pull.number]),
    })
}

/// The number of the open pull request that this pull request could join right now. The pull
/// request page offers the join only when this is set; the join applies the same rules.
pub async fn join_candidate(pool: &DbPool, pull: &PullRequest) -> anyhow::Result<Option<i32>> {
    db::pull_requests::records::join_pull_request_stack_candidate(pool, pull.id).await
}

/// After `merged` lands in `to`, the open members of its stack that are based on its head branch
/// follow it onto `to`, whose tip is `merge_sha`. Pull requests outside the stack are never
/// touched. The merge is already durable, so a failure is logged and leaves those members on their
/// old base. Returns the members that moved, as the move left them, so that their workflows can
/// run on the new base.
pub async fn retarget_after_merge(
    pool: &DbPool,
    actor_id: i64,
    merged: &PullRequest,
    to: &str,
    merge_sha: &str,
) -> Vec<PullRequest> {
    let Some(stack_id) = merged.stack_id else {
        return Vec::new();
    };
    if merged.head_repo_id != Some(merged.repo_id) {
        return Vec::new();
    }
    match db::pull_requests::records::retarget_stacked_pull_requests(
        pool,
        actor_id,
        merged.repo_id,
        stack_id,
        &merged.head_branch,
        to,
        merge_sha,
    )
    .await
    {
        Ok((moved, left)) => {
            for number in left {
                tracing::warn!(
                    pull_request_id = merged.id,
                    number,
                    "stacked pull request stayed on its old base: it would duplicate an open pull request or be based on its own head"
                );
            }
            moved
        }
        Err(error) => {
            tracing::warn!(pull_request_id = merged.id, %error, "could not retarget the stack of a merged pull request");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn pull(number: i32, head: &str, base: &str, state: &str) -> PullRequest {
        PullRequest {
            id: i64::from(number) * 10,
            repo_id: 1,
            number,
            author_id: 1,
            title: format!("layer {number}"),
            body: String::new(),
            state: state.to_owned(),
            head_repo_id: Some(1),
            workflows_approved_sha: None,
            stack_id: Some(10),
            milestone_id: None,
            head_branch: head.to_owned(),
            head_sha: "a".repeat(40),
            base_branch: base.to_owned(),
            base_sha: "b".repeat(40),
            is_draft: false,
            mergeable: None,
            merged: state == "merged",
            merged_by_id: None,
            merge_commit_sha: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            merged_at: None,
            closed_at: None,
        }
    }

    fn numbers(pulls: &[PullRequest]) -> Vec<i32> {
        pulls.iter().map(|pull| pull.number).collect()
    }

    #[test]
    fn layers_follow_the_branches_from_the_bottom_up() {
        let ordered = order_layers(vec![
            pull(3, "feature-3", "feature-2", "open"),
            pull(1, "feature-1", "main", "merged"),
            pull(2, "feature-2", "feature-1", "open"),
        ]);
        assert_eq!(numbers(&ordered), vec![1, 2, 3]);
    }

    #[test]
    fn merged_layers_stay_first_after_a_retarget_moves_the_next_layer_onto_the_trunk() {
        let ordered = order_layers(vec![
            pull(3, "feature-3", "feature-2", "open"),
            pull(2, "feature-2", "main", "open"),
            pull(1, "feature-1", "main", "merged"),
        ]);
        assert_eq!(numbers(&ordered), vec![1, 2, 3]);
    }

    #[test]
    fn a_broken_or_cyclic_chain_keeps_number_order() {
        let cyclic = order_layers(vec![
            pull(2, "feature-2", "feature-1", "open"),
            pull(1, "feature-1", "feature-2", "open"),
        ]);
        assert_eq!(numbers(&cyclic), vec![1, 2]);
        let broken = order_layers(vec![
            pull(1, "feature-1", "main", "open"),
            pull(3, "feature-3", "feature-2", "open"),
        ]);
        assert_eq!(numbers(&broken), vec![1, 3]);
    }

    #[test]
    fn merged_layers_follow_the_merge_order_not_the_numbers() {
        let mut first = pull(2, "feature-2", "main", "merged");
        first.merged_at = Some(Utc::now() - chrono::Duration::minutes(5));
        let mut second = pull(1, "feature-1", "feature-2", "merged");
        second.merged_at = Some(Utc::now());
        assert_eq!(numbers(&order_layers(vec![second, first])), vec![2, 1]);
    }

    #[test]
    fn layers_merged_in_one_batch_follow_their_branches() {
        // Merged together, so they share a time, and the upper layer has the lower number.
        let merged_at = Utc::now();
        let mut upper = pull(1, "feature-2", "feature-1", "merged");
        upper.merged_at = Some(merged_at);
        let mut lower = pull(2, "feature-1", "main", "merged");
        lower.merged_at = Some(merged_at);
        let ordered = order_layers(vec![upper, lower]);
        assert_eq!(numbers(&ordered), vec![2, 1]);
        let stack = Stack {
            id: 10,
            layers: ordered,
        };
        assert_eq!(stack.trunk(), "main");
        // The pull request ids are 10 times the numbers in `pull`: number 2 is id 20, and is first.
        assert_eq!(stack.position(20), 1);
        assert_eq!(stack.position(10), 2);
    }

    fn layer(members: &[PullRequest], number: i32) -> &PullRequest {
        members.iter().find(|pull| pull.number == number).unwrap()
    }

    /// The numbers of the open chain, bottom first, or `None` when the chain is not open.
    fn open_numbers(chain: &Chain) -> Option<Vec<i32>> {
        match chain {
            Chain::Open(layers) => Some(layers.iter().map(|pull| pull.number).collect()),
            Chain::BelowClosed(_) | Chain::Broken => None,
        }
    }

    #[test]
    fn a_closed_middle_layer_leaves_the_bottom_mergeable_and_refuses_the_top() {
        let members = vec![
            pull(1, "feature-1", "main", "open"),
            pull(2, "feature-2", "feature-1", "closed"),
            pull(3, "feature-3", "feature-2", "open"),
        ];
        let mut reversed = members.clone();
        reversed.reverse();
        for listed in [&members, &reversed] {
            assert_eq!(
                open_numbers(&chain_in(layer(listed, 1), listed)),
                Some(vec![1])
            );
            assert!(matches!(
                chain_in(layer(listed, 3), listed),
                Chain::BelowClosed(2)
            ));
        }
    }

    #[test]
    fn a_chain_lists_its_open_layers_bottom_first() {
        let members = vec![
            pull(3, "feature-3", "feature-2", "open"),
            pull(1, "feature-1", "main", "open"),
            pull(2, "feature-2", "feature-1", "open"),
        ];
        assert_eq!(
            open_numbers(&chain_in(layer(&members, 3), &members)),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            open_numbers(&chain_in(layer(&members, 2), &members)),
            Some(vec![1, 2])
        );
    }

    #[test]
    fn a_chain_stops_at_a_merged_layer() {
        let members = vec![
            pull(1, "feature-1", "main", "merged"),
            pull(2, "feature-2", "feature-1", "open"),
            pull(3, "feature-3", "feature-2", "open"),
        ];
        assert_eq!(
            open_numbers(&chain_in(layer(&members, 3), &members)),
            Some(vec![2, 3])
        );
    }

    #[test]
    fn an_open_layer_outranks_a_closed_one_on_the_same_head() {
        let members = vec![
            pull(1, "feature-1", "main", "closed"),
            pull(2, "feature-1", "main", "open"),
            pull(3, "feature-2", "feature-1", "open"),
        ];
        let below = predecessor(layer(&members, 3), &members);
        assert_eq!(below.map(|pull| pull.number), Some(2));
        assert_eq!(
            open_numbers(&chain_in(layer(&members, 3), &members)),
            Some(vec![2, 3])
        );
    }

    #[test]
    fn a_merged_layer_outranks_a_closed_one_on_the_same_head() {
        let members = vec![
            pull(1, "feature-1", "main", "closed"),
            pull(2, "feature-1", "main", "merged"),
            pull(3, "feature-2", "feature-1", "open"),
        ];
        let below = predecessor(layer(&members, 3), &members);
        assert_eq!(below.map(|pull| pull.number), Some(2));
        assert_eq!(
            open_numbers(&chain_in(layer(&members, 3), &members)),
            Some(vec![3])
        );
    }

    #[test]
    fn a_stored_cycle_is_broken_for_both_layers() {
        let cycle = vec![
            pull(1, "feature-1", "feature-2", "open"),
            pull(2, "feature-2", "feature-1", "open"),
        ];
        assert!(matches!(chain_in(layer(&cycle, 1), &cycle), Chain::Broken));
        assert!(matches!(chain_in(layer(&cycle, 2), &cycle), Chain::Broken));
    }
}
