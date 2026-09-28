use serde::{Deserialize, Serialize};

/// One source document delivered to one owner's trigger. Human-readable trigger
/// IDs are unique only within an owner; source IDs are scoped by collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FireIdentity {
    pub owner_did: String,
    pub trigger_id: String,
    pub source_collection: String,
    pub source_doc_id: String,
}

/// Admission identity is immutable and committed atomically with its AgentRequest.
/// This records delivery, not a second execution lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerFire {
    pub fire_key: String,
    #[serde(flatten)]
    pub identity: FireIdentity,
    pub task_id: String,
    pub request_id: String,
    pub session_id: String,
    pub goal_id: Option<String>,
    pub goal_objective: Option<String>,
    pub goal_token_budget: Option<i64>,
    /// Only the winning request claim may mark the queued assignment applied,
    /// in the transaction that updates its Goal and acquires execution.
    #[serde(default)]
    pub goal_assignment_applied: bool,
    pub emit_outcome: bool,
    pub queued_serial: bool,
    pub source_handoff_id: Option<String>,
    pub reply_session_id: Option<String>,
    pub shard_id: Option<String>,
    pub attempt: Option<i64>,
    pub created_at: String,
}

/// Immutable terminal observation for an opted-in fire. A goal-backed fire
/// observes its Goal assignment, never an ordinary continuing request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FireOutcome {
    pub handoff_id: String,
    pub fire_key: String,
    #[serde(flatten)]
    pub identity: FireIdentity,
    pub request_id: String,
    pub session_id: String,
    pub goal_id: Option<String>,
    pub terminal_state: String,
    pub reason: String,
    pub source_handoff_id: String,
    pub reply_session_id: Option<String>,
    pub shard_id: Option<String>,
    pub attempt: Option<i64>,
    pub created_at: String,
}

/// The first-seed exclusion set is durable. Delivery progress is reconstructed
/// from committed TriggerFire receipts, so an interrupted scan cannot skip an
/// uncommitted fire. Each trigger has an independent cursor on a shared source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSourceCursor {
    pub owner_did: String,
    pub trigger_id: String,
    pub source_collection: String,
    pub seeded_source_doc_ids: Vec<String>,
}
