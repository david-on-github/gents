import Proofs.Triggers.Durable

namespace EventDelivery.Durable

open Triggers.Durable

/-- The first seed excludes existing documents exactly once per owner/trigger/
source. Later restarts retain this baseline; disabling a trigger never reseeds.
Source documents must remain readable until delivery. Subscription notifications
only wake the existing source rescan and never advance durable progress. -/
structure Cursor where
  seeded : Bool := false
  baseline : List Identity := []
  deriving DecidableEq, Repr

def seed (cursor : Cursor) (existing : List Identity) : Cursor :=
  if cursor.seeded then cursor else { seeded := true, baseline := existing }

def pending (cursor : Cursor) (committed : State) (sourceOrder : List Identity)
    (enabled : Bool) : List Identity :=
  if !cursor.seeded || !enabled then []
  else sourceOrder.filter fun id => !cursor.baseline.contains id && !admitted committed id

theorem seed_once (cursor : Cursor) (first later : List Identity) :
    seed (seed cursor first) later = seed cursor first := by
  simp only [seed]
  split <;> simp_all

theorem disabled_does_not_deliver (cursor : Cursor) (committed : State)
    (source : List Identity) : pending cursor committed source false = [] := by
  simp [pending]

theorem restart_preserves_pending (cursor : Cursor) (committed : State)
    (source : List Identity) (h : cursor.seeded = true) :
    pending (seed cursor source) committed source true = pending cursor committed source true := by
  simp [seed, h]

/-- A rescan retains the source's order, including documents accumulated while
disabled. Committed fire receipts, rather than volatile delivery attempts, are
the authoritative cursor. Filtering therefore cannot acknowledge a failed write. -/
theorem pending_is_subsequence (cursor : Cursor) (committed : State)
    (source : List Identity) (enabled : Bool) :
    (pending cursor committed source enabled).Sublist source := by
  simp only [pending]
  split
  · exact List.nil_sublist _
  · exact List.filter_sublist source

end EventDelivery.Durable
