import Proofs.Triggers.Durable
import Proofs.EventDelivery.Durable
import Proofs.Conformance.ContractTypes

namespace Conformance.TriggerDelivery

open Triggers.Durable Conformance.Contracts

def object (fields : List (String × String)) : String :=
  "{" ++ String.intercalate "," (fields.map fun (k, v) => jsonString k ++ ":" ++ v) ++ "}"

def identity (owner document : String) : Identity :=
  { owner, document, trigger := "handoff", collection := "Work" }

def fire (document : String) : Fire :=
  { identity := identity "owner-a" document, session := "lead-session",
    serial := true, emitOutcome := true, goalBacked := false }

def identityJson (id : Identity) : String := object [
  ("owner_did", jsonString id.owner), ("trigger_id", jsonString id.trigger),
  ("source_collection", jsonString id.collection), ("source_doc_id", jsonString id.document)]

def fireJson (f : Fire) : String := object [
  ("identity", identityJson f.identity), ("session", jsonString f.session),
  ("serial", toString f.serial), ("emit_outcome", toString f.emitOutcome),
  ("goal_backed", toString f.goalBacked)]

def requestJson (r : Request) : String := object [
  ("fire", fireJson r.fire), ("running", toString r.running),
  ("terminal", toString r.terminal), ("goal_status", jsonString r.goalStatus.toDefraDB),
  ("goal_wrapup_completed", toString r.goalWrapupCompleted),
  ("goal_assignment_applied", toString r.goalAssignmentApplied)]

def stateJson (s : State) : String := object [
  ("receipts", jsonArray (s.receipts.map identityJson)),
  ("requests", jsonArray (s.requests.map requestJson)),
  ("outcomes", jsonArray (s.outcomes.map identityJson))]

def admissionCase (name : String) (pre : State) (f : Fire) (commit : Bool) : String :=
  object [("name", jsonString name), ("pre", stateJson pre), ("fire", fireJson f),
    ("commit", toString commit), ("post", stateJson (admitTransaction pre f commit))]

def admissionCases : List String := [
  admissionCase "crash_before_fire_commit" {} (fire "a") false,
  admissionCase "fire_commit" {} (fire "a") true,
  admissionCase "crash_after_fire_commit_retry" (admit {} (fire "a")) (fire "a") true,
  admissionCase "same_trigger_under_two_owners" (admit {} (fire "a"))
    { fire "a" with identity := identity "owner-b" "a" } true,
  admissionCase "same_document_other_collection" (admit {} (fire "a"))
    { fire "a" with identity := { identity "owner-a" "a" with collection := "Other" } } true,
  admissionCase "serial_burst_is_persisted" (claim (admit {} (fire "a")) (fire "a").identity)
    { fire "b" with session := "other-session" } true]

def queueCase (name : String) (pre : State) (id : Identity) : String :=
  object [("name", jsonString name), ("pre", stateJson pre),
    ("identity", identityJson id), ("can_claim", toString (canClaim pre id))]

def queueCases : List String :=
  let a := fire "a"
  let b := { fire "b" with session := "other-session" }
  let queued := admit (admit {} a) b
  let busy := claim queued a.identity
  let sameSession := { b with identity := { b.identity with trigger := "other-trigger" }, session := a.session, serial := false }
  [queueCase "serial_first_claims" queued a.identity,
   queueCase "serial_second_waits" queued b.identity,
   queueCase "serial_running_blocks_next_session" busy b.identity,
   queueCase "serial_terminal_releases_next" (terminalize busy a.identity) b.identity,
   queueCase "busy_session_queues_another_trigger" (admit busy sameSession) sameSession.identity,
   queueCase "owner_is_part_of_queue_scope"
     (admit busy { b with identity := identity "owner-b" "b" }) (identity "owner-b" "b")]

def outcomeCase (name : String) (r : Request) (already : Bool) : String :=
  let pre : State := { receipts := [r.fire.identity], requests := [r], outcomes := if already then [r.fire.identity] else [] }
  object [("name", jsonString name), ("pre", stateJson pre),
    ("due", toString (outcomeDue r)), ("post", stateJson (recoverOutcomes pre))]

def outcomeCases : List String :=
  let ordinary : Request := { fire := fire "a", terminal := true }
  let goal : Request := { ordinary with fire := { ordinary.fire with goalBacked := true }, goalAssignmentApplied := true }
  [outcomeCase "ordinary_terminal_outcome" ordinary false,
   outcomeCase "crash_after_terminal_before_outcome" ordinary false,
   outcomeCase "crash_after_outcome_retry" ordinary true,
   outcomeCase "opted_out" { ordinary with fire := { ordinary.fire with emitOutcome := false } } false,
   outcomeCase "outcome_consumer_ends_chain"
     { ordinary with fire := { ordinary.fire with emitOutcome := false, identity := { ordinary.fire.identity with collection := "FireOutcome" } } } false,
   outcomeCase "continuing_goal_has_no_outcome" goal false,
   outcomeCase "queued_goal_ignores_previous_complete" { goal with goalAssignmentApplied := false, goalStatus := .complete } false,
   outcomeCase "completed_goal" { goal with goalStatus := .complete } false,
   outcomeCase "blocked_goal" { goal with goalStatus := .blocked } false,
   outcomeCase "budget_wrapup_pending" { goal with goalStatus := .budgetLimited } false,
   outcomeCase "budget_exhausted_goal" { goal with goalStatus := .budgetLimited, goalWrapupCompleted := true } false,
   outcomeCase "paused_goal" { goal with goalStatus := .paused } false,
   outcomeCase "usage_limited_goal" { goal with goalStatus := .usageLimited } false]

def arrival (position : Nat) (document : String) : EventDelivery.Durable.Arrival :=
  { position, identity := identity "owner-a" document }

def arrivalJson (entry : EventDelivery.Durable.Arrival) : String := object [
  ("position", jsonString (toString entry.position)), ("identity", identityJson entry.identity)]

def cursorJson (cursor : EventDelivery.Durable.Cursor) : String := object [
  ("seeded", toString cursor.seeded), ("after", jsonString (toString cursor.after))]

structure CursorScenario where
  name : String
  seedHead : Nat := 1
  priorAfter : Option Nat := none
  restart : Bool := false
  enabled : Bool := true
  committed : State := {}
  entry : EventDelivery.Durable.Arrival := arrival 2 "b"
  matchesFilter : Bool := true
  admissionCommit : Option Bool := none
  checkpointCommit : Option Bool := none

def cursorCase (scenario : CursorScenario) : String :=
  let source := [arrival 1 "a", arrival 2 "b", arrival 3 "c"]
  let first := EventDelivery.Durable.seed {} scenario.seedHead
  let saved := match scenario.priorAfter with
    | none => first
    | some position => EventDelivery.Durable.advance first position true
  let before := if scenario.restart then EventDelivery.Durable.seed saved 3 else saved
  let f := { fire scenario.entry.identity.document with identity := scenario.entry.identity }
  let afterState := match scenario.admissionCommit with
    | none => scenario.committed
    | some commit => admitTransaction scenario.committed f commit
  let afterCursor := match scenario.checkpointCommit with
    | none => before
    | some commit => EventDelivery.Durable.acknowledge before afterState scenario.entry scenario.matchesFilter commit
  let optionalBool := fun value : Option Bool => match value with
    | none => "null"
    | some value => toString value
  object [
    ("name", jsonString scenario.name), ("seed_head", jsonString (toString scenario.seedHead)),
    ("restart", toString scenario.restart), ("enabled", toString scenario.enabled),
    ("pre_cursor", cursorJson before), ("post_cursor", cursorJson afterCursor),
    ("pre", stateJson scenario.committed), ("post", stateJson afterState),
    ("source", jsonArray (source.map arrivalJson)), ("entry", arrivalJson scenario.entry),
    ("fire", fireJson f), ("matches_filter", toString scenario.matchesFilter),
    ("admission_commit", optionalBool scenario.admissionCommit),
    ("checkpoint_commit", optionalBool scenario.checkpointCommit),
    ("checkpoint_succeeds", optionalBool (scenario.checkpointCommit.map fun commit => commit && (!scenario.matchesFilter || admitted afterState scenario.entry.identity))),
    ("acknowledgment_allowed", toString (!scenario.matchesFilter || admitted afterState scenario.entry.identity)),
    ("pending", jsonArray ((EventDelivery.Durable.pending afterCursor source scenario.enabled).map arrivalJson)),
    ("journal_after", jsonArray ((EventDelivery.Durable.pending afterCursor source true).map arrivalJson))]

def cursorCases : List String :=
  [({ name := "first_seed_excludes_existing", seedHead := 3 } : CursorScenario),
   { name := "first_seed_disabled_retains_later_arrivals", enabled := false },
   { name := "reenabled_delivers_receiving_order" },
   { name := "restart_does_not_reseed", restart := true },
   { name := "crash_before_fire_commit", admissionCommit := some false, checkpointCommit := some true },
   { name := "crash_after_fire_commit_before_checkpoint", admissionCommit := some true },
   { name := "checkpoint_transaction_crashes", admissionCommit := some true, checkpointCommit := some false },
   { name := "replay_after_receipt_commit", committed := admit {} (fire "b"), admissionCommit := some true, checkpointCommit := some true },
   { name := "checkpoint_commits", admissionCommit := some true, checkpointCommit := some true },
   { name := "replay_after_checkpoint_commit", priorAfter := some 2, committed := admit {} (fire "b"), admissionCommit := some true, checkpointCommit := some true },
   { name := "unmatched_filter_checkpoint", matchesFilter := false, checkpointCommit := some true },
   { name := "stale_checkpoint_keeps_progress", priorAfter := some 3, committed := admit {} (fire "b"), checkpointCommit := some true },
   { name := "unadmitted_match_cannot_checkpoint", checkpointCommit := some true }].map cursorCase

def identityCases : List String :=
  [identity "owner-a" "a", identity "owner-b" "a", identity "a:b" "é",
   { identity "a" "b:é" with trigger := "b:handoff" }].map fun id => object [
    ("identity", identityJson id), ("key", jsonString id.key),
    ("request_id", jsonString id.requestId), ("session_id", jsonString id.sessionId),
    ("outcome_id", jsonString id.outcomeId)]

def sessionCases : List String :=
  let id := identity "owner-a" "a"
  [(none, true), (some "lead-one", true), (some "lead-two", true),
   (some "", true), (some "foreign", false)].map fun (target, owned) => object [
     ("identity", identityJson id), ("target", jsonOptionalString target),
     ("owned", toString owned), ("resolved", jsonOptionalString (resolveSession id target owned))]

def claimObservationJson (row : ClaimObservation) : String := object [
  ("document", jsonString row.document), ("owner", jsonString row.owner),
  ("session", jsonString row.session), ("trigger", jsonString row.trigger),
  ("serial", toString row.serial), ("receipt", toString row.receipt),
  ("arrival", row.arrival.map toString |>.getD "null"),
  ("running", toString row.running), ("terminal", toString row.terminal)]

def observedClaimCases : List String :=
  let old : ClaimObservation := { document := "old", owner := "owner", session := "session" }
  let next : ClaimObservation := { old with document := "new", arrival := some 1, receipt := true }
  let cases : List (String × ClaimObservation × List ClaimObservation) := [
    ("historical_pending_unordered", old, [{ old with document := "older" }]),
    ("historical_running_blocks", old, [{ old with document := "older", running := true }]),
    ("historical_precedes_journal", next, [old]),
    ("journal_does_not_precede_historical", old, [next]),
    ("journal_running_blocks_historical", old, [{ next with running := true }]),
    ("unrelated_historical_does_not_block", next, [{ old with session := "elsewhere" }]),
    ("foreign_owner_does_not_block", next, [{ old with owner := "other" }]),
    ("receipt_missing_position_rejected", { next with arrival := none }, []),
    ("receipt_conflict_missing_position_rejected", old, [{ next with arrival := none }]),
    ("native_pending_fifo", { next with arrival := some 2 }, [{ next with document := "prior" }]),
    ("terminal_historical_does_not_block", next, [{ old with terminal := true }])]
  cases.map fun (name, candidate, rows) => object [
    ("name", jsonString name), ("candidate", claimObservationJson candidate),
    ("rows", jsonArray (rows.map claimObservationJson)),
    ("allowed", toString (observedClaimAllowed candidate rows))]

def casesJson : String := object [
  ("admissions", jsonArray admissionCases), ("queues", jsonArray queueCases),
  ("observed_claims", jsonArray observedClaimCases),
  ("outcomes", jsonArray outcomeCases), ("cursors", jsonArray cursorCases),
  ("identities", jsonArray identityCases), ("sessions", jsonArray sessionCases)]

example : (admit (admit {} (fire "a")) (fire "a")).requests.length = 1 := by native_decide
example : outcomeDue { fire := { fire "a" with goalBacked := true }, terminal := true } = false := by
  native_decide

end Conformance.TriggerDelivery
