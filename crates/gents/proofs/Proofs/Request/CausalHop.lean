import Proofs.Basic

/-!
# Causal hop: the only loop bound between agents

Every agent is an ordinary agent, addressed directly; the runtime encodes no
parent/child hierarchy. The one causal fact the runtime keeps is the hop: the
signed, immutable `AgentRequest.subagent_depth`, written once by
`lifecycle::materialize` and checked at admission against the target's
`AgentPrincipal.max_request_hop`.

Each session has a *current hop*: the hop of its latest request. A request
(or continuation) caused by *another* session's action is strictly further
than its cause: `hop = max current (cause + 1)`. That covers a tool-caused new
request, a tool-caused steering continuation and a session-message completion
wake (whose cause is the caused request that finished). Same-session
continuations (retries, goal continuations, user steering, native process
completion wakes) copy the session's current hop, never the hop of an older
request that scheduled them. A user-authored root request is hop zero and so
resets its session. There is no separate refusal path: a request over the
bound is written like any other, becomes its session's latest request, and is
refused at admission, so every later same-session continuation copies a hop
over the bound and is refused too; the session waits for its user. Every chain of agent-to-agent causes
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

/-- The hop a request is materialized with, given its session's current hop
(the hop of its latest request; `0` for a new session). -/
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
the completion wake it returns) continues the other session, whose current hop
is that of the request two links back. -/
def pingPongHops (n : Nat) : List Nat :=
  (List.range n).foldl
    (fun hops _ =>
      let own := (hops.reverse.drop 1).head?.getD 0
      let cause := hops.getLast?.getD 0
      hops ++ [nextHop (.crossSession cause) own])
    [0]

/-! ## Sessions, wakes and refusal

A session's durable state for the hop bound is its current hop and the one
pending wake the next claim will run. A completion's notification is appended
before its wake is enqueued, and the next wake claim consumes every pending
notification of the session, so the pending wake always carries the highest
hop any of them requires: a wake at a higher hop supersedes a lower pending
wake, and a lower one joins the pending wake. -/

structure WakeSession where
  currentHop : Nat
  pendingWake : Option Nat
  deriving DecidableEq, Repr

/-- Enqueue a completion wake at `hop`: join a pending wake at least as high,
otherwise supersede it. A new wake is the session's latest request. -/
def WakeSession.enqueue (s : WakeSession) (hop : Nat) : WakeSession :=
  match s.pendingWake with
  | some pending => if hop ≤ pending then s else ⟨max s.currentHop hop, some hop⟩
  | none => ⟨max s.currentHop hop, some hop⟩

/-- A session-message completion wake, caused by a request at `causeHop`. -/
def WakeSession.crossWake (s : WakeSession) (causeHop : Nat) : WakeSession :=
  s.enqueue (nextHop (.crossSession causeHop) s.currentHop)

/-- A native process (or Goal) completion wake: copies the current hop. -/
def WakeSession.nativeWake (s : WakeSession) : WakeSession :=
  s.enqueue (nextHop .continuation s.currentHop)

/-- The next claim runs the pending wake if admission admits its hop; either
way the pending wake is consumed (admitted, or terminally refused). -/
def WakeSession.claim (maxHop : Nat) (s : WakeSession) : Option Bool × WakeSession :=
  (s.pendingWake.map (admitHop maxHop), { s with pendingWake := none })

/-- A same-session continuation never lowers the current hop. -/
theorem nativeWake_keeps_current (s : WakeSession) :
    s.currentHop ≤ s.nativeWake.currentHop := by
  unfold WakeSession.nativeWake WakeSession.enqueue
  cases h : s.pendingWake <;> simp [nextHop] <;> split <;> simp_all

/-- Once the current hop is over the bound, every later native or Goal wake
is refused: a refused session waits for its user. -/
theorem over_bound_session_refuses_native_wakes (maxHop : Nat) (s : WakeSession)
    (h_over : maxHop < s.currentHop) (h_none : s.pendingWake = none) :
    (s.nativeWake.claim maxHop).1 = some false := by
  simp [WakeSession.nativeWake, WakeSession.enqueue, WakeSession.claim, h_none,
    nextHop, admitHop]
  omega

/-- A refused cross-session wake with a lower native wake pending: the pending
wake is superseded by the refused one, so the lower wake never runs the
refused result. -/
theorem refused_cross_wake_supersedes_pending_native_wake :
    let s : WakeSession := ⟨3, some 3⟩
    (s.crossWake 8).pendingWake = some 9 ∧
      ((s.crossWake 8).claim defaultMaxRequestHop).1 = some false := by
  native_decide

/-- A native wake enqueued after the refused cross-session wake was claimed
copies the refused hop and is refused as well. -/
theorem later_native_wake_after_refusal_is_refused :
    let s : WakeSession := (((⟨3, some 3⟩ : WakeSession).crossWake 8).claim
      defaultMaxRequestHop).2
    (s.nativeWake.claim defaultMaxRequestHop).1 = some false := by
  native_decide

/-- The reviewer's loop: A messages B and starts a background process each
turn; B's completion wakes A, and so does A's process. Each round records
whether B's request, A's completion wake and A's process wake are admitted.
A turn of A runs only when one of its wakes is admitted. -/
structure LoopState where
  a : WakeSession
  b : Nat
  aRuns : Bool
  deriving Repr

def loopRound (maxHop : Nat) (st : LoopState) : LoopState × List Bool :=
  if !st.aRuns then (st, []) else
    let b := nextHop (.crossSession st.a.currentHop) st.b
    let bAdmitted := admitHop maxHop b
    -- B's request ends (run or refused); its completion wakes A.
    let (crossAdmitted, a) := (st.a.crossWake b).claim maxHop
    -- A's background process from the same turn completes afterwards.
    let (nativeAdmitted, a) := a.nativeWake.claim maxHop
    let crossAdmitted := crossAdmitted.getD false
    let nativeAdmitted := nativeAdmitted.getD false
    (⟨a, b, crossAdmitted || nativeAdmitted⟩, [bAdmitted, crossAdmitted, nativeAdmitted])

def loopTrace (maxHop : Nat) : Nat → LoopState → List (List Bool)
  | 0, _ => []
  | n + 1, st =>
    let (next, admitted) := loopRound maxHop st
    admitted :: loopTrace maxHop n next

/-- The A↔B loop with a native process wake halts at the bound: after the
fifth round every request in the loop is refused, and nothing runs again. -/
theorem ping_pong_halts_at_max :
    let trace := loopTrace defaultMaxRequestHop 8 ⟨⟨0, none⟩, 0, true⟩
    trace.take 4 = List.replicate 4 [true, true, true] ∧
      trace[4]? = some [false, false, false] ∧
      (trace.drop 5).all List.isEmpty = true := by
  native_decide

end CausalHop
