use super::*;
use workspace_lineage::{
    select_graph_session, session_target_eligible, SessionContinuationContext,
    SessionRootCandidate, SessionTargetEligibility, SessionTargetRoute,
};

fn number(value: &Value) -> String {
    value.as_u64().unwrap().to_string()
}
fn boolean(value: &Value) -> bool {
    value.as_bool().unwrap()
}

#[test]
fn session_selection_matches_executable_graph_owner() {
    let contract = gents_lean_contract::load_contract_snapshot::<Value>().unwrap();
    for case in contract["graph_session_continuation_cases"]
        .as_array()
        .unwrap()
    {
        let raw = &case["eligibility"];
        let eligibility = SessionTargetEligibility {
            source_is_task: boolean(&raw["source_is_task"]),
            target_exists: boolean(&raw["target_exists"]),
            target_is_task: boolean(&raw["target_is_task"]),
            route_count: raw["route_count"].as_u64().unwrap() as usize,
            route_kind: match raw["route_kind"].as_str().unwrap() {
                "selected_entry" => SessionTargetRoute::SelectedEntry,
                "grouped" => SessionTargetRoute::Grouped,
                "per_document" => SessionTargetRoute::PerDocument,
                _ => panic!("unknown generated route kind"),
            },
        };
        assert_eq!(
            session_target_eligible(&eligibility),
            boolean(&case["eligible"]),
            "{}",
            case["name"]
        );
        let raw = &case["context"];
        let context = SessionContinuationContext {
            owner: number(&case["owner"]),
            firing_node: number(&case["firing_node"]),
            target_node: number(&case["target_node"]),
            correlation: number(&raw["correlation"]),
            revision: number(&raw["revision"]),
            target_route: number(&raw["target_route"]),
            run_and_plan_verified: boolean(&raw["run_and_plan_verified"]),
            destination_route_verified: boolean(&raw["destination_route_verified"]),
        };
        let candidates = case["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| {
                let root = &candidate["root"];
                SessionRootCandidate {
                    root_doc_id: number(&root["doc_id"]),
                    session_id: number(&candidate["session"]),
                    owner: number(&candidate["owner"]),
                    correlation: number(&root["correlation"]),
                    revision: number(&root["revision"]),
                    target_route: number(&root["entry_route"]),
                    authenticated: boolean(&root["authenticated_target"]),
                }
            })
            .collect::<Vec<_>>();
        let actual = select_graph_session(&eligibility, &context, &candidates);
        if case["expected"].is_null() {
            assert!(actual.is_none(), "{}", case["name"]);
        } else {
            let actual = actual.expect("modeled session selection accepted");
            assert_eq!(actual.session_id, number(&case["expected"]["session"]));
            assert_eq!(actual.root_doc_id, number(&case["expected"]["root_doc"]));
            assert_eq!(actual.firing_node, number(&case["expected"]["firing_node"]));
        }
    }
}

#[test]
fn applied_assignment_heads_match_executable_graph_owner() {
    let contract = gents_lean_contract::load_contract_snapshot::<Value>().unwrap();
    for case in contract["graph_assignment_head_cases"].as_array().unwrap() {
        let heads = case["heads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|head| logical_invocation::AssignmentHead {
                member: boolean(&head["member"]),
                authentic_root: boolean(&head["authentic_root"]),
                assignment_applied: boolean(&head["assignment_applied"]),
                authenticated_continuation: boolean(&head["authenticated_continuation"]),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            logical_invocation::assignment_owns_goal(
                boolean(&case["root_assignment_applied"]),
                &heads
            ),
            boolean(&case["expected"]),
            "{}",
            case["name"]
        );
    }
}
