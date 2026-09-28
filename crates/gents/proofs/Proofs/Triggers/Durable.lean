import Proofs.Triggers.Types
import Proofs.Goals

namespace Triggers.Durable

/-- Trigger IDs are only unique within their owner. Source document identity
is scoped by collection, including when two sources share a trigger name. -/
structure Identity where
  owner : String
  trigger : String
  collection : String
  document : String
  deriving DecidableEq, Repr

def component (value : String) : String := s!"{value.length}:{value}"

def Identity.key (id : Identity) : String :=
  component id.owner ++ component id.trigger ++ component id.collection ++ component id.document

def Identity.requestId (id : Identity) : String := "trigger-request:" ++ id.key

def Identity.sessionId (id : Identity) : String := "trigger-session:" ++ id.key

def Identity.outcomeId (id : Identity) : String := "outcome:" ++ id.key

structure Fire where
  identity : Identity
  session : String
  serial : Bool
  emitOutcome : Bool
  goalBacked : Bool
  deriving DecidableEq, Repr

structure Request where
  fire : Fire
  running : Bool := false
  terminal : Bool := false
  goalStatus : Goals.Status := .active
  goalWrapupCompleted : Bool := false
  deriving DecidableEq, Repr

/-- Pending serial fires are ordinary persisted requests. The request claim
owner admits only the earliest pending request for the trigger and session;
dispatch never drops a fire merely because another request is running. -/
structure State where
  receipts : List Identity := []
  requests : List Request := []
  outcomes : List Identity := []
  deriving DecidableEq, Repr

def admitted (state : State) (id : Identity) : Bool := decide (id ∈ state.receipts)

/-- The database transaction publishes the receipt and request together. A
crash before commit preserves the pre-state; a crash after commit preserves
both. Recovery repeats this same admission operation. -/
def admit (state : State) (fire : Fire) : State :=
  if admitted state fire.identity then state
  else { state with
    receipts := state.receipts ++ [fire.identity]
    requests := state.requests ++ [{ fire := fire }] }

theorem admit_duplicate (state : State) (fire : Fire)
    (h : admitted state fire.identity = true) : admit state fire = state := by
  simp [admit, h]

theorem admit_idempotent (state : State) (fire : Fire) :
    admit (admit state fire) fire = admit state fire := by
  by_cases h : fire.identity ∈ state.receipts
  · simp [admit, admitted, h]
  · simp [admit, admitted, h]

theorem admission_has_request (state : State) (fire : Fire)
    (h : admitted state fire.identity = false) :
    (admit state fire).requests = state.requests ++ [{ fire := fire }] := by
  simp [admit, h]

theorem admission_has_receipt (state : State) (fire : Fire) :
    admitted (admit state fire) fire.identity = true := by
  by_cases h : fire.identity ∈ state.receipts
  · simp [admit, admitted, h]
  · simp [admit, admitted, h]

def conflicts (candidate other : Request) : Bool :=
  candidate.fire.identity.owner == other.fire.identity.owner &&
    (candidate.fire.session == other.fire.session ||
      (candidate.fire.serial && other.fire.identity.trigger == candidate.fire.identity.trigger))

def canClaim (state : State) (id : Identity) : Bool :=
  match state.requests.find? (fun r => r.fire.identity == id) with
  | none => false
  | some candidate =>
      !candidate.running && !candidate.terminal &&
      !(state.requests.any (fun other =>
        other.running && !other.terminal && conflicts candidate other)) &&
      !(state.requests.takeWhile (fun other => other.fire.identity != id)).any
        (fun other => !other.terminal && conflicts candidate other)

def claim (state : State) (id : Identity) : State :=
  if canClaim state id then
    { state with requests := state.requests.map fun r =>
      if r.fire.identity == id then { r with running := true } else r }
  else state

theorem blocked_claim_preserves_queue (state : State) (id : Identity)
    (h : canClaim state id = false) : claim state id = state := by
  simp [claim, h]

theorem admission_preserves_outcomes (state : State) (fire : Fire) :
    (admit state fire).outcomes = state.outcomes := by
  simp only [admit]
  split <;> rfl

/-- A pre-commit crash discards all staged writes. An acknowledgement lost after
commit is retried through the same identity, with no second request. -/
def admitTransaction (state : State) (fire : Fire) (commit : Bool) : State :=
  if commit then admit state fire else state

theorem admission_crash_before_commit (state : State) (fire : Fire) :
    admitTransaction state fire false = state := rfl

theorem admission_crash_after_commit (state : State) (fire : Fire) :
    admitTransaction (admitTransaction state fire true) fire true =
      admitTransaction state fire true := admit_idempotent state fire

/-- Temporary pauses and provider usage limits preserve the assignment's final
outcome. The caller is notified when the Goal completes, blocks, or exhausts its
budget; ordinary request boundaries and resumable stops cannot consume it. -/
def goalEnded (status : Goals.Status) (wrapupCompleted : Bool) : Bool :=
  status == .complete || status == .blocked || (status == .budgetLimited && wrapupCompleted)

def outcomeDue (request : Request) : Bool :=
  request.fire.emitOutcome &&
    (if request.fire.goalBacked then goalEnded request.goalStatus request.goalWrapupCompleted else request.terminal)

def publishOutcome (state : State) (request : Request) : State :=
  if outcomeDue request && !(decide (request.fire.identity ∈ state.outcomes)) then
    { state with outcomes := state.outcomes ++ [request.fire.identity] }
  else state

def recoverOutcomes (state : State) : State :=
  state.requests.foldl publishOutcome state

def terminalize (state : State) (id : Identity) : State :=
  recoverOutcomes { state with requests := state.requests.map fun request =>
    if request.fire.identity == id then { request with running := false, terminal := true }
    else request }

def setGoal (state : State) (id : Identity) (status : Goals.Status) : State :=
  recoverOutcomes { state with requests := state.requests.map fun request =>
    if request.fire.identity == id then { request with goalStatus := status }
    else request }

theorem ordinary_goal_boundary_no_outcome (request : Request)
    (hg : request.fire.goalBacked = true) (ha : request.goalStatus = .active) :
    outcomeDue request = false := by
  simp [outcomeDue, hg, goalEnded, ha]

theorem opted_out_no_outcome (request : Request)
    (h : request.fire.emitOutcome = false) : outcomeDue request = false := by
  simp [outcomeDue, h]

theorem outcome_idempotent (state : State) (request : Request) :
    publishOutcome (publishOutcome state request) request = publishOutcome state request := by
  simp only [publishOutcome]
  split
  · simp [List.mem_append]
  · rename_i h
    simp [h]

/-- A configured destination must resolve to a session of the same owner and
behavior. Being busy does not invalidate it; the request claim queue owns that
occupancy. The chosen ID is fixed before either Task template is rendered. -/
def resolveSession (id : Identity) (target : Option String)
    (ownedSameBehavior : Bool) : Option String :=
  match target with
  | none => some id.sessionId
  | some value => if value.isEmpty || !ownedSameBehavior then none else some value

def isCurrent (caller listed : String) : Bool := caller == listed

theorem current_session_marks_self (session : String) : isCurrent session session = true := by
  simp [isCurrent]

/-- Session observation and Task delivery use the same resolved destination,
including when another request currently owns that session's execution lease. -/
theorem existing_session_retained (id : Identity) (session : String)
    (h : session.isEmpty = false) : resolveSession id (some session) true = some session := by
  simp [resolveSession, h]

theorem foreign_session_rejected (id : Identity) (session : String) :
    resolveSession id (some session) false = none := by
  simp [resolveSession]

structure GoalAssignment where
  state : Goals.State
  epoch : Nat
  deriving DecidableEq, Repr

/-- An authenticated Task assignment advances the existing Goal controller
epoch. Explicit task assignment may replace a completed or exhausted goal;
model-facing create/update/resume retain their existing authority and rules.
Prior assignment outcomes must be published before this operation commits. -/
def assignGoal (previous : Option GoalAssignment) : GoalAssignment :=
  { state := { status := .active, blockedAudits := 0,
               wrapupRequested := false, wrapupCompleted := false }
    epoch := previous.map (fun g => g.epoch + 1) |>.getD 0 }

theorem task_assignment_active (previous : Option GoalAssignment) :
    (assignGoal previous).state.status = .active := rfl

theorem task_assignment_invalidates_previous_controller (previous : GoalAssignment) :
    (assignGoal (some previous)).epoch ≠ previous.epoch := by
  simp [assignGoal]

end Triggers.Durable
