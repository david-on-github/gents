import Proofs.Basic

/-!
# Causal hop: the only loop bound between agents

Every agent is an ordinary agent, addressed directly; the runtime encodes no
parent/child hierarchy. The one causal fact the runtime keeps is the hop: the
signed, immutable `AgentRequest.subagent_depth`, written once by
`lifecycle::materialize` and checked at admission against the target's
`AgentPrincipal.max_request_hop`.

A request (or continuation) caused by *another* session's action is strictly
further than its cause: `hop = max own (cause + 1)`, where `own` is the hop of
the request it continues in its own session. That covers a tool-caused new
request, a tool-caused steering continuation and a session-message completion
wake (whose cause is the caused request that finished). Same-session
continuations (retries, goal continuations, user steering, native process
completion wakes) copy their predecessor. Every chain of agent-to-agent causes
therefore climbs by at least one per send, so a loop between agents — two
sessions messaging each other, or one agent waking itself through a session
it started — is refused after a bounded number of sends, with no cascade,
fence or tree walk. An `agent_message` to the caller's own current session is
refused outright: it would be a same-session steering continuation with no
hop increase.
-/

namespace CausalHop

/-- Default `AgentPrincipal.max_request_hop` when the principal leaves it unset. -/
def defaultMaxRequestHop : Nat := 8

/-- Why a request exists, relative to the request whose hop it inherits. -/
inductive Cause where
  /-- A user, trigger or schedule root. -/
  | root
  /-- Caused by another session's action at hop `causeHop`: a
  `agent_new`/`agent_message` request or steering continuation (the cause
  is the calling request), or a session-message completion wake (the cause is
  the caused request that finished). -/
  | crossSession (causeHop : Nat)
  /-- A same-session continuation: retry, goal continuation, user steering or
  a native process completion wake. It cannot extend a chain. -/
  | continuation
  deriving DecidableEq, Repr

/-- The hop a request is materialized with, given the hop of the request it
continues in its own session (`0` for a new session). -/
def nextHop : Cause → Nat → Nat
  | .root, _ => 0
  | .crossSession causeHop, own => max own (causeHop + 1)
  | .continuation, own => own

/-- Admission refuses a request whose hop exceeds the target principal's
`max_request_hop`. The check reads only the signed hop; no lineage walk,
replication of the predecessor, or cooperation of the sender is needed. -/
def admitHop (maxHop hop : Nat) : Bool := decide (hop ≤ maxHop)

/-- `agent_message` never addresses the caller's own current session. -/
def sendTargetAllowed (callerSession targetSession : String) : Bool :=
  callerSession != targetSession

theorem root_hop_is_zero (own : Nat) : nextHop .root own = 0 := rfl

theorem continuation_preserves_hop (own : Nat) :
    nextHop .continuation own = own := rfl

/-- A cross-session cause is strictly behind what it causes, whatever the
target session's own history. -/
theorem cross_session_exceeds_cause (causeHop own : Nat) :
    causeHop + 1 ≤ nextHop (.crossSession causeHop) own := by
  simp only [nextHop]; omega

/-- A cross-session cause never lowers the target session's own hop. -/
theorem cross_session_keeps_own (causeHop own : Nat) :
    own ≤ nextHop (.crossSession causeHop) own := by
  simp only [nextHop]; omega

theorem send_to_own_session_refused (session : String) :
    sendTargetAllowed session session = false := by
  simp [sendTargetAllowed]

/-- One link of a causal chain across sessions: the next request is either
caused by the current one from another session (with its own session's
predecessor hop), or continues it in the same session. -/
inductive Step where
  | cross (ownPredecessorHop : Nat)
  | cont
  deriving DecidableEq, Repr

def Step.apply : Step → Nat → Nat
  | .cross own, hop => nextHop (.crossSession hop) own
  | .cont, hop => nextHop .continuation hop

/-- The hop reached after a chain of steps from a starting hop. -/
def hopAlong (start : Nat) : List Step → Nat
  | [] => start
  | step :: rest => hopAlong (step.apply start) rest

/-- Number of agent-to-agent sends in a chain. -/
def sends : List Step → Nat
  | [] => 0
  | .cross _ :: rest => sends rest + 1
  | .cont :: rest => sends rest

theorem hopAlong_ge (start : Nat) (steps : List Step) :
    start + sends steps ≤ hopAlong start steps := by
  induction steps generalizing start with
  | nil => simp [hopAlong, sends]
  | cons step rest ih =>
    cases step with
    | cross own =>
      have h := ih (Step.apply (.cross own) start)
      have h_step : start + 1 ≤ Step.apply (.cross own) start := by
        simp only [Step.apply, nextHop]; omega
      simp only [hopAlong, sends]
      omega
    | cont =>
      simpa [hopAlong, sends, Step.apply, nextHop] using ih start

/-- Every admitted request has at most `maxHop` agent-to-agent sends in its
causal chain, across any sessions and interleaved continuations: `maxHop` is
the `max_request_hop` of the principal that admits it. There is no global
constant; `defaultMaxRequestHop` only fills an unset field. -/
theorem admitted_chain_sends_le_max (maxHop start : Nat) (steps : List Step)
    (h_admit : admitHop maxHop (hopAlong start steps) = true) :
    sends steps ≤ maxHop := by
  have h := hopAlong_ge start steps
  simp [admitHop] at h_admit
  omega

/-- A message loop is cut: once a chain holds more sends than the admitting
target's bound, its request is refused. -/
theorem send_beyond_max_is_refused (maxHop start : Nat) (steps : List Step)
    (h_sends : maxHop < sends steps) :
    admitHop maxHop (hopAlong start steps) = false := by
  have h := hopAlong_ge start steps
  simp [admitHop]
  omega

/-- Continuations never change admissibility: retries, goal continuations,
user steering and native completion wakes of an admitted request stay
admitted. -/
theorem continuation_preserves_admission (maxHop hop : Nat) :
    admitHop maxHop (nextHop .continuation hop) = admitHop maxHop hop := rfl

/-- Two sessions messaging each other from a root at hop zero: each send (or
the completion wake it returns) continues the other session, whose own
predecessor is the request two links back. -/
def pingPongHops (n : Nat) : List Nat :=
  (List.range n).foldl
    (fun hops _ =>
      let own := (hops.reverse.drop 1).head?.getD 0
      let cause := hops.getLast?.getD 0
      hops ++ [nextHop (.crossSession cause) own])
    [0]

/-- The A↔B loop halts at the bound: under the default bound exactly eight
sends are admitted and the ninth is refused. -/
theorem ping_pong_halts_at_max :
    ((pingPongHops 9).drop 1).map (admitHop defaultMaxRequestHop) =
      [true, true, true, true, true, true, true, true, false] := by
  native_decide

/-- What a session-message completion does when its wake would exceed the
woken principal's bound: the notification is still appended, so the result is
never lost, and only the wake is refused with a visible reason; the session
waits for its user. The next wake claim consumes every pending notification
of the session, so the one pending wake carries the highest hop they require:
a lower pending wake is superseded by one at the higher hop, never joined. -/
inductive CompletionWake where
  | notifyAndWake
  | notifyOnlyHopBound
  deriving DecidableEq, Repr

def completionWake (maxHop wakeHop : Nat) : CompletionWake :=
  if admitHop maxHop wakeHop then .notifyAndWake else .notifyOnlyHopBound

def CompletionWake.notifies : CompletionWake → Bool
  | .notifyAndWake => true
  | .notifyOnlyHopBound => true

theorem completion_result_always_delivered (maxHop wakeHop : Nat) :
    (completionWake maxHop wakeHop).notifies = true := by
  unfold completionWake; split <;> rfl

theorem completion_wake_refused_iff_beyond_bound (maxHop wakeHop : Nat) :
    completionWake maxHop wakeHop = .notifyOnlyHopBound ↔ maxHop < wakeHop := by
  unfold completionWake admitHop
  by_cases h : wakeHop ≤ maxHop <;> simp [h] <;> omega

end CausalHop
