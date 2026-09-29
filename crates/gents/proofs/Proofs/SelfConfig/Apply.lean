import Proofs.SelfConfig.Types

namespace SelfConfig

abbrev FieldValue := String

def Doc := FieldKey → Option FieldValue

def Doc.ofList (entries : List (FieldKey × FieldValue)) : Doc :=
  fun k => (entries.find? (fun e => e.1 == k)).map (·.2)

inductive PatchOp where
  | set (value : FieldValue)
  | clear
  deriving DecidableEq, Repr

def PatchOp.value : PatchOp → Option FieldValue
  | .set v => some v
  | .clear => none

structure PatchEntry where
  key : FieldKey
  op : PatchOp
  deriving DecidableEq, Repr

abbrev Patch := List PatchEntry

def applyEntry (t : Target) (doc : Doc) (e : PatchEntry) : Doc :=
  if e.key ∈ writableFields t then
    fun k => if k = e.key then e.op.value else doc k
  else
    doc

def applyPatch (t : Target) (doc : Doc) (p : Patch) : Doc :=
  p.foldl (applyEntry t) doc

def admissible (t : Target) (p : Patch) : Bool :=
  p.all (fun e => decide (e.key ∈ writableFields t))

def step (validate guard : Doc → Bool) (t : Target) (stored : Doc)
    (p : Patch) : Option Doc :=
  if admissible t p = true then
    if (validate (applyPatch t stored p) && guard (applyPatch t stored p))
        = true then
      some (applyPatch t stored p)
    else
      none
  else
    none

def Store := Target → Doc

def runStep (validate guard : Doc → Bool) (t : Target) (s : Store)
    (p : Patch) : Store × Bool :=
  match step validate guard t (s t) p with
  | some merged => (fun t' => if t' = t then merged else s t', true)
  | none => (s, false)

/-- The invoker's retained control over itself, projected from its candidate
Tools by the canonical typed decoder: `self_config.enable_self_config` and
`subagents.enabled` (absent is false). Decode errors must fail the shared
validator; this model does not parse or duplicate the nested schema. -/
structure Control where
  selfConfig : Bool
  agents : Bool
  deriving DecidableEq, Repr

/-- No lockout (#1796) is the only self-protection. The Engineer is a full
self-writing agent: it may edit its own Tools and target itself with
automation, and every such write is checked the normal way (preview, ACP, typed
validation). It is refused only a candidate that turns off its self-config tool
or removes an agents tool group it already had. Behavior and backend
enablement are the same invariant on the other reference-chain documents and
remain their existing typed guards. -/
def keepsControl (decode : Doc → Option Control) (stored candidate : Doc) : Bool :=
  match decode stored, decode candidate with
  | some old, some new => new.selfConfig && (!old.agents || new.agents)
  | _, _ => false

end SelfConfig
