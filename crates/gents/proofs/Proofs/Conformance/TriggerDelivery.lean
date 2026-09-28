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

def cursorCases : List String :=
  let a := identity "owner-a" "a"
  let b := identity "owner-a" "b"
  let c := identity "owner-a" "c"
  let baseline := EventDelivery.Durable.seed {} [a]
  [("first_seed", baseline, ({} : State), [a], true),
   ("disabled_waits", baseline, {}, [a,b,c], false),
   ("reenabled_preserves_order", baseline, {}, [a,b,c], true),
   ("restart_preserves_seed", EventDelivery.Durable.seed baseline [a,b,c], {}, [a,b,c], true),
   ("committed_fire_is_cursor", baseline, admit {} (fire "b"), [a,b,c], true)].map
    fun (name, cursor, committed, source, enabled) => object [
      ("name", jsonString name), ("seeded", toString cursor.seeded),
      ("baseline", jsonArray (cursor.baseline.map identityJson)),
      ("committed", jsonArray (committed.receipts.map identityJson)),
      ("source", jsonArray (source.map identityJson)), ("enabled", toString enabled),
      ("pending", jsonArray ((EventDelivery.Durable.pending cursor committed source enabled).map identityJson))]

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

def casesJson : String := object [
  ("admissions", jsonArray admissionCases), ("queues", jsonArray queueCases),
  ("outcomes", jsonArray outcomeCases), ("cursors", jsonArray cursorCases),
  ("identities", jsonArray identityCases), ("sessions", jsonArray sessionCases)]

example : (admit (admit {} (fire "a")) (fire "a")).requests.length = 1 := by native_decide
example : outcomeDue { fire := { fire "a" with goalBacked := true }, terminal := true } = false := by
  native_decide

end Conformance.TriggerDelivery
