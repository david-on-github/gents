namespace StoreEncryptionUpgrade

inductive Phase where
  | copying | verified | installed
  deriving DecidableEq, Repr

inductive Action where
  | copy | retireSource | promote | recordInstalled | open | refuse
  deriving DecidableEq, Repr

/-- The caller excludes every store opener throughout conversion. The journal
is synced before any key materialization or copy. `verified` is persisted only
after every raw source key/value equals the decrypted destination, host sidecars
have been successfully copied, and both stores are closed. Power-loss recovery
assumes directory renames and journal writes sync their parent directory before
proceeding (the Unix adapter); other native targets guarantee only process-restart
recovery. `source` excludes an empty
directory recreated by host startup between source retirement and promotion;
native recovery removes that placeholder with nonrecursive `remove_dir`. -/
def next (phase : Phase) (source stage retired : Bool) : Action :=
  match phase, source, stage, retired with
  | .copying, true, _, false => .copy
  | .verified, true, true, false => .retireSource
  | .verified, false, true, true => .promote
  | .verified, true, false, true => .recordInstalled
  | .installed, true, false, _ => .open
  | _, _, _, _ => .refuse

/-- Removing the retired plaintext and journal is permitted only after the
enclosing home/client metadata durably records the same encryption key and the
native node/schema owners accept the destination. Raw KV equality alone cannot
distinguish valid plaintext from an unrecorded encrypted store. These observations
are supplied by the host owners after conversion, not inferred from raw bytes. -/
def mayFinish (phase : Phase) (metadataCommitted nativeAccepted : Bool) : Bool :=
  phase == .installed && metadataCommitted && nativeAccepted

/-- An older binary may have written plaintext after a crash released the
host lock. Recheck under the new exclusion before retiring that source. -/
def afterRecheck (equal : Bool) : Phase := if equal then .verified else .copying

theorem changed_source_restarts_copy : afterRecheck false = .copying := rfl

theorem promotion_requires_verification (p : Phase) (a b c : Bool)
    (h : next p a b c = .promote) : p = .verified := by
  cases p <;> cases a <;> cases b <;> cases c <;> simp_all [next]

theorem copying_never_opens (a b c : Bool) : next .copying a b c ≠ .open := by
  cases a <;> cases b <;> cases c <;> decide

theorem unfinished_metadata_keeps_journal (p : Phase) (accepted : Bool) :
    mayFinish p false accepted = false := by
  cases p <;> rfl

theorem refused_native_store_keeps_journal (p : Phase) (committed : Bool) :
    mayFinish p committed false = false := by
  cases p <;> cases committed <;> rfl

end StoreEncryptionUpgrade
