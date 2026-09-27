import Proofs.Recovery.Sweeps.ToolCalls

namespace Recovery

open ToolExecution

/-- An `agent_new`/`agent_message` row ends on the terminal of the request
    it caused, or fails closed when it cannot name that request. No parent
    terminal is a cause, because the started session is an ordinary agent's
    session, not a subordinate; and the row carries no deadline, because
    nothing waits on it and the caused request's result is always delivered
    to the calling session.

    The row turns running in the same transaction that writes its caused
    request and the receipt naming it, so a running row always has both.
    `causedRequestUnbound` is the verdict for a row whose receipt is missing
    or names a request that fails the row's lineage: it can never settle from
    a terminal, so it fails closed once instead of being observed forever.

    Accepted premise: a peer that never replicates its caused request's
    terminal back leaves the row running. There is no deadline for that case;
    a kill always ends the row (`killAction`). -/
inductive SessionMessageRecoveryCause where
  | causedRequestUnbound
  | requestCompleted
  | requestFailed
  | requestDead
  | requestInterrupted
  | requestSuperseded
  deriving DecidableEq, Repr

namespace SessionMessageRecoveryCause

def toContract : SessionMessageRecoveryCause → String
  | .causedRequestUnbound => "causedRequestUnbound"
  | .requestCompleted => "requestCompleted"
  | .requestFailed => "requestFailed"
  | .requestDead => "requestDead"
  | .requestInterrupted => "requestInterrupted"
  | .requestSuperseded => "requestSuperseded"

def terminalState : SessionMessageRecoveryCause → ToolCallState
  | .causedRequestUnbound => .failed
  | .requestCompleted => .completed
  | .requestFailed => .failed
  | .requestDead => .failed
  | .requestInterrupted => .cancelled
  | .requestSuperseded => .failed

theorem terminalState_terminal (cause : SessionMessageRecoveryCause) :
    isTerminal cause.terminalState := by
  cases cause <;>
    simp [terminalState, HasTerminal.isTerminal, ToolCallState.instHasTerminal]

end SessionMessageRecoveryCause

structure SessionMessageRecoveryRow where
  call : ToolCallContext
  cause : SessionMessageRecoveryCause
  deriving Repr

/-- Follows the existing `toolCallRecoverySweep` pattern: the row carries the
cause observed by the recovering owner (the caused request reached a durable
terminal, or the row cannot name it), and staleness is only the running
session-message shape. Recovery never invents a cause; a row with neither
observation is not submitted to this sweep. -/
def sessionMessageRecoveryStale (row : SessionMessageRecoveryRow) : Prop :=
  row.call.state = .running ∧ isSessionMessageCall row.call

instance (row : SessionMessageRecoveryRow) : Decidable (sessionMessageRecoveryStale row) := by
  unfold sessionMessageRecoveryStale
  infer_instance

def sessionMessageRecover (row : SessionMessageRecoveryRow) : SessionMessageRecoveryRow :=
  { row with call := { row.call with state := row.cause.terminalState } }

def sessionMessageRecoveryMeasure (row : SessionMessageRecoveryRow) : Nat :=
  if sessionMessageRecoveryStale row then 1 else 0

theorem sessionMessageRecovery_stale_positive :
    ∀ row, sessionMessageRecoveryStale row → sessionMessageRecoveryMeasure row > 0 := by
  intro row h_stale
  simp [sessionMessageRecoveryMeasure, h_stale]

theorem sessionMessageRecover_terminal :
    ∀ row, sessionMessageRecoveryStale row → isTerminal (sessionMessageRecover row).call.state := by
  intro row _h_stale
  rcases row with ⟨call, cause⟩
  cases cause <;>
    simp [sessionMessageRecover, SessionMessageRecoveryCause.terminalState,
      HasTerminal.isTerminal, ToolCallState.instHasTerminal]

theorem sessionMessageRecover_zero :
    ∀ row, sessionMessageRecoveryStale row → sessionMessageRecoveryMeasure (sessionMessageRecover row) = 0 := by
  intro row _h_stale
  have h_terminal_not_running : row.cause.terminalState ≠ .running := by
    cases row.cause <;> simp [SessionMessageRecoveryCause.terminalState]
  have h_not : ¬ sessionMessageRecoveryStale (sessionMessageRecover row) := by
    intro h_stale
    rcases h_stale with ⟨h_running, _h_session⟩
    simp [sessionMessageRecover] at h_running
    exact h_terminal_not_running h_running
  simp [sessionMessageRecoveryMeasure, h_not]

/-- What a kill (`cancel_process` or the operator kill) observes about the
    request a running session-message row caused. -/
inductive KillObservation where
  /-- The caused request already reached a durable terminal. -/
  | causedTerminal
  /-- The caused request is live and this runtime executes it. -/
  | causedLiveLocal
  /-- The caused request is live on a peer. -/
  | causedLiveRemote
  /-- The receipt or the caused request is missing, unreadable, or fails the
  row's lineage. -/
  | unresolved
  deriving DecidableEq, Repr

inductive KillAction where
  /-- Settle from the observed terminal, delivering its result. -/
  | settle
  /-- Interrupt the caused request; its terminal, which this runtime writes,
  settles the row. -/
  | interruptCaused
  /-- Ask the peer to interrupt, and cancel the row now. -/
  | interruptAndCancelRow
  /-- Cancel the row now with a cancelled completion notification. -/
  | cancelRow
  deriving DecidableEq, Repr

def killAction : KillObservation → KillAction
  | .causedTerminal => .settle
  | .causedLiveLocal => .interruptCaused
  | .causedLiveRemote => .interruptAndCancelRow
  | .unresolved => .cancelRow

/-- Whether the kill ends the row itself rather than handing it to a terminal
    this runtime will write. -/
def KillAction.endsRowNow : KillAction → Bool
  | .interruptCaused => false
  | _ => true

/-- A kill always ends the row: immediately, or through the caused request's
    terminal that this runtime itself writes. It never waits on a peer or on a
    request it cannot name. -/
theorem kill_waits_only_on_a_local_terminal (obs : KillObservation) :
    (killAction obs).endsRowNow = false ↔ obs = .causedLiveLocal := by
  cases obs <;> simp [killAction, KillAction.endsRowNow]

def sessionMessageRecoverySweep : RecoverySweep :=
  { Row := SessionMessageRecoveryRow
  , collection := .agentToolCall
  , sweepId := "tool_call_lifecycle_recover_session_message_rows"
  , rustFunction := "ToolCallLifecycle::recover_all"
  , cadence := .startup
  , implementationStatus := .implemented
  , stale := sessionMessageRecoveryStale
  , recover := sessionMessageRecover
  , terminal := fun row => isTerminal row.call.state
  , measure := sessionMessageRecoveryMeasure
  , h_stale_positive := sessionMessageRecovery_stale_positive
  , h_recover_terminal := sessionMessageRecover_terminal
  , h_recover_zero := sessionMessageRecover_zero
  }

end Recovery
