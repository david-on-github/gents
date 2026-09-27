import Proofs.EventDelivery.Contract
import Proofs.EventDelivery.Properties

open EventDelivery

namespace EventDelivery.Watcher

def watcherSrc : SourceInstance :=
  { name := "Watcher"
  , dedupePolicy := .ttlCooldown
  , rescanBoundedBy := 1
  }

theorem watcher_pending_eventually_observed
    (w₀ : World) (d : DocId)
    (h_persisted : d ∈ w₀.persistentSet)
    (h_unprocessed : d ∉ w₀.processedSet) :
    ∃ (actions : List Action) (w' : World),
      TraceOf w₀ actions w' ∧
      Fair watcherSrc actions ∧
      d ∈ w'.handled :=
  D1_delivery_convergence watcherSrc w₀ d h_persisted h_unprocessed (by decide)

/-- Exclusion holds while the cooldown entry is present in this epoch. Runtime
expiry/eviction permits another delivery and is not modeled as permanent dedupe. -/
theorem watcher_cooldown_excludes_handle
    (w : World) (d : DocId) (a : Action) (w' : World)
    (h_processed : d ∈ w.processedSet)
    (h : Transition w a w') :
    a ≠ .handle d :=
  C1_processed_set_excludes_handle w d a w' h_processed h

/-- Whether delivering `head` releases the cooldown mark of `d`.

Premise: the watcher delivers only the first pending request of a session in
`(created_at, request_id)` order, and `created_at` has whole-second
resolution. A request created later in the same second with a smaller
`request_id` (a `background-completion-…` wake after a `goal-cont-…`
continuation) therefore becomes the session head after the earlier request
was already delivered. That earlier request's claim then observes a
different earliest pending row, returns `Queued`, and stays pending with its
cooldown mark held. Delivering the new head is the observation that the
earlier delivery was overtaken, so it releases the marks of the other rows in
that session. A duplicate delivery is harmless: the claim is a CAS on the
pending lifecycle state. -/
def releasedBy (session : DocId → String) (head d : DocId) : Prop :=
  d ≠ head ∧ session d = session head

/-- An overtaken request keeps its mark until released: no delivery at all. -/
theorem watcher_marked_request_not_redelivered
    (w : World) (d : DocId) (a : Action) (w' : World)
    (h_marked : d ∈ w.processedSet)
    (h : Transition w a w') :
    a ≠ .handle d :=
  C1_processed_set_excludes_handle w d a w' h_marked h

/-- Once the overtaking head releases it and the head leaves the pending set
(its terminal transition unblocks the session), the next rescan delivers the
overtaken request within `rescanBoundedBy`. -/
theorem watcher_overtaken_request_redelivered_after_unblock
    (session : DocId → String) (w₀ : World) (head d : DocId)
    (h_released : releasedBy session head d)
    (h_pending : d ∈ w₀.persistentSet)
    (h_head : head ∈ w₀.persistentSet)
    (h_marked : d ∈ w₀.processedSet) :
    ∃ (w₂ w' : World),
      TraceOf w₀ [.release d, .depersist head] w₂ ∧
      TraceOf w₂ [.rescanTick, .handle d] w' ∧
      Fair watcherSrc [.rescanTick, .handle d] ∧
      d ∈ w'.handled := by
  let w₁ : World :=
    { w₀ with processedSet := w₀.processedSet.filter (fun x => x ≠ d) }
  let w₂ : World :=
    { w₁ with persistentSet := w₁.persistentSet.erase head }
  have h_unmarked : d ∉ w₂.processedSet := by
    show d ∉ w₀.processedSet.filter (fun x => x ≠ d)
    simp
  have h_still_pending : d ∈ w₂.persistentSet :=
    (List.mem_erase_of_ne h_released.1).mpr h_pending
  let w₃ : World :=
    { w₂ with subscriptionQueue :=
        (w₂.persistentSet.filter (fun x => x ∉ w₂.processedSet)) ++ w₂.subscriptionQueue }
  let w₄ : World :=
    { w₃ with handled := d :: w₃.handled
            , processedSet := d :: w₃.processedSet
            , subscriptionQueue := w₃.subscriptionQueue.erase d }
  have h_queued : d ∈ w₃.subscriptionQueue := by
    show d ∈ (w₂.persistentSet.filter (fun x => x ∉ w₂.processedSet)) ++ w₂.subscriptionQueue
    apply List.mem_append.mpr
    left
    apply List.mem_filter.mpr
    refine ⟨h_still_pending, ?_⟩
    simp [h_unmarked]
  refine ⟨w₂, w₄, ?_, ?_, ?_, ?_⟩
  · exact TraceOf.cons (Transition.release w₀ d h_marked)
      (TraceOf.cons (Transition.depersist w₁ head h_head) TraceOf.nil)
  · exact TraceOf.cons (Transition.rescanTick w₂)
      (TraceOf.cons (Transition.handle w₃ d h_queued h_unmarked) TraceOf.nil)
  · intro i h_window
    have h_len : ([Action.rescanTick, Action.handle d]).length = 2 := rfl
    have h_b : watcherSrc.rescanBoundedBy = 1 := rfl
    rw [h_len, h_b] at h_window
    have h_i : i = 0 := by omega
    subst h_i
    exact ⟨0, Nat.le_refl 0, by rw [h_b]; omega, rfl⟩
  · exact List.mem_cons_self _ _

/-- Release leaves the head's own mark and other sessions' marks in place. -/
theorem watcher_release_keeps_unrelated_marks
    (w : World) (d x : DocId) (w' : World)
    (h : Transition w (.release d) w')
    (h_other : x ≠ d)
    (h_marked : x ∈ w.processedSet) :
    x ∈ w'.processedSet := by
  cases h
  show x ∈ w.processedSet.filter (fun y => y ≠ d)
  simp [h_marked, h_other]

end EventDelivery.Watcher
