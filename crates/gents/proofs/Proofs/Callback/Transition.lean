import Proofs.Callback.Types

namespace CallbackInvocation

/-- Recovery found this running invocation cut off mid-attempt. Every action
still executing is marked interrupted, so the journal keeps the fact that an
unknown effect may have happened. -/
def interruptJournal (journal : List ActionJournalEntry) : List ActionJournalEntry :=
  journal.map fun e => { e with state := ActionJournalState.markInterrupted e.state }

/-- A failed invocation may run again while it has attempts left and nothing
it did could repeat: no action observed its effect, wrote results or was
interrupted with an unknown outcome. -/
def retryAllowed (inv : CallbackInvocation) (maxAttempts : Nat) : Bool :=
  decide (inv.state = .failed) && decide (inv.attempts < maxAttempts) &&
    inv.journal.all (fun e => !ActionJournalState.effectful e.state)

inductive Transition : CallbackInvocation → CallbackInvocation → Prop where
  | claim {pre post : CallbackInvocation} :
      pre.state = .pending →
      post = { pre with state := .claimed, attempts := pre.attempts + 1 } →
      Transition pre post
  | run {pre post : CallbackInvocation} :
      pre.state = .claimed →
      post = { pre with state := .running } →
      Transition pre post
  | succeed {pre post : CallbackInvocation} :
      pre.state = .running →
      pre.journal.all (fun e => decide (e.state = .resultDocsWritten)) = true →
      post = { pre with state := .succeeded, resultEmitted := true } →
      Transition pre post
  /-- The attempt observed its own failure, so an action it leaves `executing`
  returned without an effect the runtime could see. Recovery never takes this
  step: it cannot observe the attempt it found, and fails it only by `recover`. -/
  | fail {pre post : CallbackInvocation} :
      pre.state = .running →
      post = { pre with state := .failed, resultEmitted := false } →
      Transition pre post
  /-- Recovery's failure of an attempt it found cut off; see `recover`. -/
  | interrupt {pre post : CallbackInvocation} :
      pre.state = .running →
      post = { pre with
        state := .failed, journal := interruptJournal pre.journal, resultEmitted := false } →
      Transition pre post
  | deny_claimed {pre post : CallbackInvocation} :
      pre.state = .claimed →
      pre.journal = [] →
      post = { pre with state := .denied, resultEmitted := false } →
      Transition pre post
  | deny_running {pre post : CallbackInvocation} :
      pre.state = .running →
      pre.journal = [] →
      post = { pre with state := .denied, resultEmitted := false } →
      Transition pre post
  | retry {pre post : CallbackInvocation} (maxAttempts : Nat) :
      retryAllowed pre maxAttempts = true →
      post = { pre with state := .pending, journal := [], resultEmitted := false } →
      Transition pre post

/-- What recovery does with an invocation it finds. A running invocation whose
journal is non-empty was cut off mid-attempt: recovery cannot observe what its
actions did, so it fails it with executing actions marked interrupted and
never with a plain `fail`. A running invocation with an empty journal did
nothing and its attempt carries on; other states are left to their owners. -/
def recover (inv : CallbackInvocation) : CallbackInvocation :=
  if inv.state = .running ∧ inv.journal ≠ [] then
    { inv with state := .failed, journal := interruptJournal inv.journal, resultEmitted := false }
  else inv

/-- What a denial does with a running invocation, such as recovery meeting a
disabled callback or an illegal journal. Before any action started it is denied
with an empty journal. After, the host may already have acted and the denial
cannot observe what it did, so it fails the invocation exactly as `recover`
does: a denial is never a way around marking an attempt interrupted. -/
def deny (inv : CallbackInvocation) : CallbackInvocation :=
  if inv.journal = [] then { inv with state := .denied, resultEmitted := false }
  else recover inv

end CallbackInvocation
