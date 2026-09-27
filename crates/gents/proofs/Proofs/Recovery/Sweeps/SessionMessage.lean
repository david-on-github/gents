import Proofs.Recovery.Sweeps.ToolCalls

namespace Recovery

open ToolExecution

/-- An `agent_new`/`agent_message` row ends on the terminal of the request
    it caused, or fails closed when it cannot name that request
    (`ToolRecoveryCause.causedRequestUnbound`). No parent terminal is a cause,
    because the started session is an ordinary agent's session, not a
    subordinate; and the row carries no deadline, because nothing waits on it
    and the caused request's result is always delivered to the calling
    session. The sweep runs on the periodic tick, which also covers startup.

    Accepted premise: a peer that never replicates its caused request's
    terminal back leaves the row running. There is no deadline for that case;
    a kill always ends the row (`killAction`). -/
def sessionMessageRecoverySweep : RecoverySweep :=
  toolCallRecoverySweepFor true
    "tool_call_lifecycle_recover_session_message_rows"
    "background_completion::settle_running_session_message_rows" .periodic

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

def KillObservation.toContract : KillObservation → String
  | .causedTerminal => "causedTerminal"
  | .causedLiveLocal => "causedLiveLocal"
  | .causedLiveRemote => "causedLiveRemote"
  | .unresolved => "unresolved"

def KillAction.toContract : KillAction → String
  | .settle => "settle"
  | .interruptCaused => "interruptCaused"
  | .interruptAndCancelRow => "interruptAndCancelRow"
  | .cancelRow => "cancelRow"

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

/-- Every kill observation with the action the kill takes on it. -/
def killCases : List (KillObservation × KillAction) :=
  [.causedTerminal, .causedLiveLocal, .causedLiveRemote, .unresolved].map
    fun obs => (obs, killAction obs)

end Recovery
