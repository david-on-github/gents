import Proofs.Triggers.Durable

namespace EventDelivery.Durable

open Triggers.Durable

/-- DefraDB supplies a durable first-arrival sequence per receiving node and
collection, including replicated arrivals. The native page is in increasing
sequence order; this is not a global creation or wall-clock order. Each physical
document appears once. Source documents must remain readable until delivery. -/
structure Arrival where
  position : Nat
  identity : Identity
  deriving DecidableEq, Repr

/-- Each owner/trigger/source cursor is seeded at the native journal head when
its Trigger configuration is created, including disabled Triggers. Progress is
node-local and never replicates with source configuration. Restart and
re-enable retain that checkpoint. Decimal-string native positions decode to Nat;
subscription notifications only wake reads and never advance durable progress. -/
structure Cursor where
  seeded : Bool := false
  after : Nat := 0
  deriving DecidableEq, Repr

def seed (cursor : Cursor) (head : Nat) : Cursor :=
  if cursor.seeded then cursor else { seeded := true, after := head }

def pending (cursor : Cursor) (sourceOrder : List Arrival)
    (enabled : Bool) : List Arrival :=
  if cursor.seeded && enabled then sourceOrder.filter fun entry => cursor.after < entry.position
  else []

/-- Native progress writes are monotonic and transactional. The caller has either
excluded every intervening arrival by its filter or admitted its fire; advancing
to a later arbitrary journal position is not an admissible scan operation. -/
def advance (cursor : Cursor) (position : Nat) (checkpointCommitted : Bool) : Cursor :=
  if cursor.seeded && checkpointCommitted then
    { cursor with after := max cursor.after position }
  else cursor

/-- Matching arrivals acknowledge the authoritative receipt, committed atomically
with its request. A conclusively unmatched arrival needs no receipt. If admission
commits but checkpointing crashes, the journal replays the same arrival and the
existing receipt refuses a second request before checkpointing can complete. -/
def acknowledge (cursor : Cursor) (committed : State) (entry : Arrival)
    (matchesFilter checkpointCommitted : Bool) : Cursor :=
  if matchesFilter && !admitted committed entry.identity then cursor
  else advance cursor entry.position checkpointCommitted

theorem seed_once (cursor : Cursor) (first later : Nat) :
    seed (seed cursor first) later = seed cursor first := by
  simp only [seed]
  split <;> simp_all

theorem disabled_does_not_deliver (cursor : Cursor) (source : List Arrival) :
    pending cursor source false = [] := by
  simp [pending]

theorem restart_preserves_pending (cursor : Cursor) (source : List Arrival)
    (head : Nat) (h : cursor.seeded = true) :
    pending (seed cursor head) source true = pending cursor source true := by
  simp [seed, h]

theorem pending_is_subsequence (cursor : Cursor) (source : List Arrival) (enabled : Bool) :
    (pending cursor source enabled).Sublist source := by
  simp only [pending]
  split
  · exact List.filter_sublist source
  · exact List.nil_sublist _

theorem advance_never_regresses (cursor : Cursor) (position : Nat) (commit : Bool) :
    cursor.after ≤ (advance cursor position commit).after := by
  simp only [advance]
  split
  · exact Nat.le_max_left _ _
  · exact Nat.le_refl _

theorem failed_checkpoint_preserves_cursor (cursor : Cursor) (committed : State)
    (entry : Arrival) (matchesFilter : Bool) :
    acknowledge cursor committed entry matchesFilter false = cursor := by
  simp [acknowledge, advance]

theorem unadmitted_match_cannot_advance (cursor : Cursor) (committed : State)
    (entry : Arrival) (commit : Bool) (h : admitted committed entry.identity = false) :
    acknowledge cursor committed entry true commit = cursor := by
  simp [acknowledge, h]

end EventDelivery.Durable
