import Proofs.Basic
import Proofs.Request.CausalHop

/-!
# Durable request lineage

The database is the control plane, so logical identifiers are useful labels
but document identifiers are the authoritative edges.  This model describes
the ingest boundary for request lineage:

* logical and physical halves of an edge are either both present or absent;
* a request is a root, a session-message request (the full calling request
  and tool call edge written by `agent_new`/`agent_message`), or an
  explicitly marked request-only control continuation;
* malformed replicated rows are rejected individually, without preventing a
  later well-formed row from being considered; and
* queued steering admission retains signed raw input; canonical publication of
  the prepared message belongs to the owned execution start, not this lineage
  ingest boundary (see `QueuedSteering`).

`subagentDepth` is the causal hop (`CausalHop`). A session-message request and
every continuation caused by another session's action (an agent-authored
steering continuation, a session-message completion wake) take
`max own (cause + 1)`; same-session continuations keep their predecessor's
hop (`ContinuationKind.hop`). The edge is provenance only; it grants no
hierarchy, cascade or authority over the calling session.
-/

namespace DurableLineage

structure RawLineage where
  hasParentRequestId : Bool
  hasParentRequestDocId : Bool
  hasParentToolCallId : Bool
  hasParentToolCallDocId : Bool
  subagentDepth : Nat
  requestOnlyControl : Bool
  controlAllowedAtDepthZero : Bool := false
  deriving DecidableEq, Repr

def pairCoherent (logical physical : Bool) : Bool := logical == physical

def edgePairsCoherent (row : RawLineage) : Bool :=
  pairCoherent row.hasParentRequestId row.hasParentRequestDocId &&
    pairCoherent row.hasParentToolCallId row.hasParentToolCallDocId

def parentShapeCoherent (row : RawLineage) : Bool :=
  let root := !row.hasParentRequestId && !row.hasParentToolCallId
  let bridge := row.hasParentRequestId && row.hasParentToolCallId
  let control :=
    row.requestOnlyControl && row.hasParentRequestId && !row.hasParentToolCallId
  root || bridge || control

def depthCoherent (row : RawLineage) : Bool :=
  if row.hasParentRequestId then
    row.subagentDepth > 0 ||
      (row.requestOnlyControl && row.controlAllowedAtDepthZero)
  else
    row.subagentDepth == 0

def admissible (row : RawLineage) : Bool :=
  edgePairsCoherent row && parentShapeCoherent row && depthCoherent row

def admissibleRows (rows : List RawLineage) : List RawLineage :=
  rows.filter admissible

/-- A bad replicated/foreign row is skipped at the ingest boundary instead of
    poisoning every valid request behind it in the watcher batch. -/
theorem malformed_head_does_not_poison
    (bad : RawLineage)
    (rest : List RawLineage)
    (hBad : admissible bad = false) :
    admissibleRows (bad :: rest) = admissibleRows rest := by
  simp [admissibleRows, hBad]

def steeringContinuation (subagentDepth : Nat) : RawLineage :=
  { hasParentRequestId := true
  , hasParentRequestDocId := true
  , hasParentToolCallId := false
  , hasParentToolCallDocId := false
  , subagentDepth
  , requestOnlyControl := true
  , controlAllowedAtDepthZero := true
  }

/-- Steering is request-linked, not a new send.  Normalization keeps
    both halves of the parent request edge and clears both halves of the old
    tool-call bridge. -/
theorem steering_continuation_is_admissible
    (depth : Nat) :
    admissible (steeringContinuation depth) = true := by
  simp [admissible, edgePairsCoherent, pairCoherent, parentShapeCoherent,
    depthCoherent, steeringContinuation]

def backgroundCompletionContinuation (subagentDepth : Nat) : RawLineage :=
  { hasParentRequestId := true
  , hasParentRequestDocId := true
  , hasParentToolCallId := false
  , hasParentToolCallDocId := false
  , subagentDepth
  , requestOnlyControl := true
  , controlAllowedAtDepthZero := true
  }

/-- A background-completion wake is a control continuation, not a new
    send.  It therefore preserves the parent's depth, including
    depth zero for a top-level or goal-continuation session. -/
theorem background_completion_continuation_is_admissible
    (depth : Nat) :
    admissible (backgroundCompletionContinuation depth) = true := by
  simp [admissible, edgePairsCoherent, pairCoherent, parentShapeCoherent,
    depthCoherent, backgroundCompletionContinuation]

def goalContinuation (subagentDepth : Nat) : RawLineage :=
  { hasParentRequestId := true
  , hasParentRequestDocId := true
  , hasParentToolCallId := false
  , hasParentToolCallDocId := false
  , subagentDepth
  , requestOnlyControl := true
  , controlAllowedAtDepthZero := true
  }

/-- A durable-goal continuation preserves the hop and carries both the
    logical and physical parent request edge. It is controller work, not a new
    send. Session and behavior preservation are runtime request-
    construction obligations outside `RawLineage`. -/
theorem goal_continuation_is_admissible (depth : Nat) :
    admissible (goalContinuation depth) = true := by
  simp [admissible, edgePairsCoherent, pairCoherent, parentShapeCoherent,
    depthCoherent, goalContinuation]

/-- The request-only continuations. Two are caused by another session's
action and carry that cause's hop: an `agent_message` steering a busy session
(the calling request) and a session-message completion wake (the caused
request that finished). The rest continue their own session's work. -/
inductive ContinuationKind where
  | userSteering
  | agentSteering (callerHop : Nat)
  | retry
  | goal
  | nativeCompletionWake
  | sessionMessageCompletionWake (causedHop : Nat)
  deriving DecidableEq, Repr

def ContinuationKind.cause : ContinuationKind → CausalHop.Cause
  | .agentSteering callerHop => .crossSession callerHop
  | .sessionMessageCompletionWake causedHop => .crossSession causedHop
  | _ => .continuation

/-- The hop a continuation is written with, from its session's current hop
`own` (the hop of the session's latest request, not of an older request that
scheduled the continuation). -/
def ContinuationKind.hop (kind : ContinuationKind) (own : Nat) : Nat :=
  CausalHop.nextHop kind.cause own

/-- Every continuation keeps the request-only control shape at any hop. -/
theorem continuation_lineage_is_admissible (kind : ContinuationKind) (own : Nat) :
    admissible (steeringContinuation (kind.hop own)) = true :=
  steering_continuation_is_admissible _

/-- Continuations caused by another session climb past that cause, so they
cannot carry an agent loop past the admitting bound. -/
theorem cross_session_continuations_climb (own causeHop : Nat) :
    causeHop + 1 ≤ (ContinuationKind.agentSteering causeHop).hop own ∧
      causeHop + 1 ≤ (ContinuationKind.sessionMessageCompletionWake causeHop).hop own :=
  ⟨CausalHop.cross_session_exceeds_cause causeHop own,
    CausalHop.cross_session_exceeds_cause causeHop own⟩

/-- Same-session continuations copy their predecessor's hop. -/
theorem own_session_continuations_copy (own : Nat) :
    ContinuationKind.userSteering.hop own = own ∧ ContinuationKind.retry.hop own = own ∧
      ContinuationKind.goal.hop own = own ∧
      ContinuationKind.nativeCompletionWake.hop own = own := by
  simp [ContinuationKind.hop, ContinuationKind.cause, CausalHop.nextHop]

/-- Interrupting another agent's session — `agent_message` with
    `interrupt`, or `agent_interrupt` — is allowed in 0.20 only to the session
    that started it: the target session's origin (its first public request's
    `caused_by_parent_*`, read through `gents::session_origin`) names the
    caller's session. `targetOriginCause` is that origin's calling session,
    `none` for a root session. General interrupt permissions are deferred to a
    later release. -/
def interruptAllowed (callerSession targetSession : String)
    (targetOriginCause : Option String) : Bool :=
  targetSession != callerSession && targetOriginCause == some callerSession

/-- A caller that did not start the target session is refused. -/
theorem non_spawner_interrupt_refused (callerSession targetSession : String)
    (targetOriginCause : Option String)
    (h : targetOriginCause ≠ some callerSession) :
    interruptAllowed callerSession targetSession targetOriginCause = false := by
  simp [interruptAllowed, h]

/-- The session that started another session may interrupt it. -/
theorem spawner_may_interrupt (callerSession targetSession : String)
    (h : targetSession ≠ callerSession) :
    interruptAllowed callerSession targetSession (some callerSession) = true := by
  simp [interruptAllowed, h]

/-- A root session, which no session started, cannot be interrupted by an
    agent. -/
theorem root_session_not_interruptible (callerSession targetSession : String) :
    interruptAllowed callerSession targetSession none = false := by
  simp [interruptAllowed]

end DurableLineage
