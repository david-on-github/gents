import Proofs.GraphPipeline.WorkspaceLineage
import Proofs.Conformance.Contracts.Json.Helpers

namespace Conformance.GraphWorkspaceLineageContracts
open GraphPipeline GraphPipeline.WorkspaceLineage Conformance.Contracts
open Conformance.ContractCases

def identity : Identity := ⟨11, 21, some 31⟩
def context : Context := ⟨1, 2, 3, true, true, some .readOnly⟩
def root : Root := ⟨41, 1, 2, 3, true, true, some identity⟩
def state : LogicalInvocation.PublicationState :=
  ⟨⟨⟨1, 1, 1, 2, .running, true, false, false⟩, 4, none⟩, 0⟩
def bound : Resolved := ⟨some identity, some .readOnly⟩

structure Case where
  name : String
  ctx : Context := context
  source : Source := .downstream [root]
  explicit : Explicit := {}
  before : LogicalInvocation.PublicationState := state
  expectedGeneration : Nat := 4
  expected : Option Resolved := none
  published : Bool := false
  deriving DecidableEq, Repr

def cases : List Case :=
  [ {name := "entry_bootstrap_owner_stamped_seal", source := .bootstrap
        {workspaceId := some 11} (some identity) true true,
      expected := some bound, published := true}
  , {name := "area_to_scan_inherits_entry", expected := some bound, published := true}
  , {name := "scan_to_verifier_partial_matching_context", explicit := {workspaceId := some 11},
      expected := some bound, published := true}
  , {name := "unbound_entry_stays_unbound", source := .downstream [{root with workspace := none}],
      expected := some ⟨none, some .readOnly⟩, published := true}
  , {name := "bound_entry_to_no_workspace_triage", ctx := {context with destinationAuthority := none},
      expected := some ⟨none, none⟩, published := true}
  , {name := "unbound_generic_graph_no_authority", ctx := {context with destinationAuthority := none},
      source := .downstream [{root with workspace := none}],
      expected := some ⟨none, none⟩, published := true}
  , {name := "no_authority_matching_identity_is_attenuated", ctx := {context with destinationAuthority := none},
      explicit := {workspaceId := some 11, sealHash := some 31},
      expected := some ⟨none, none⟩, published := true}
  , {name := "no_authority_rejects_explicit_authority", ctx := {context with destinationAuthority := none},
      explicit := {authority := some .readOnly}}
  , {name := "no_authority_rejects_conflicting_identity", ctx := {context with destinationAuthority := none},
      explicit := {workspaceId := some 99}}
  , {name := "explicit_workspace_conflict", explicit := {workspaceId := some 99}}
  , {name := "bootstrap_unverified_workspace_owner", source := .bootstrap
        {workspaceId := some 11} (some identity) true false}
  , {name := "explicit_seal_conflict", explicit := {sealHash := some 99}}
  , {name := "explicit_authority_conflict", explicit := {authority := some .readWrite}}
  , {name := "missing_admitted_entry", source := .downstream []}
  , {name := "ambiguous_authenticated_entries", source := .downstream [root,{root with docId := 42}]}
  , {name := "untrusted_root_cannot_grant", source := .downstream [{root with authenticatedTarget := false}]}
  , {name := "untrusted_lookalike_ignored", source := .downstream
        [root,{root with docId := 42, authenticatedTarget := false, workspace := some ⟨99,99,some 99⟩}],
      expected := some bound, published := true}
  , {name := "wrong_root_revision", source := .downstream [{root with revision := 99}]}
  , {name := "wrong_root_correlation", source := .downstream [{root with correlation := 99}]}
  , {name := "destination_not_pinned_route", ctx := {context with destinationRouteVerified := false}}
  , {name := "bootstrap_controller_input_conflict", source := .bootstrap
        {workspaceId := some 99} (some identity) true true}
  , {name := "unbound_entry_rejects_injected_workspace", source := .downstream [{root with workspace := none}],
      explicit := {workspaceId := some 11}}
  , {name := "stale_publication_generation", expectedGeneration := 3, expected := some bound}
  , {name := "canceled_run_blocks_publication", before :=
        {state with graph := {state.graph with run := {state.graph.run with cancellationRequested := true}}},
      expected := some bound}
  , {name := "latched_failure_blocks_publication", before :=
        {state with graph := {state.graph with primary := some 9}}, expected := some bound}
  ]

theorem all_cases_agree : cases.all (fun c =>
    resolve c.ctx c.source c.explicit == c.expected &&
    ((publish c.before c.expectedGeneration c.ctx c.source c.explicit).children > c.before.children) == c.published) = true := by
  decide

def optionalIdentityJson : Option Identity → String
  | none => "null"
  | some i => "{\"workspace_id\":" ++ toString i.workspaceId ++
      ",\"owner\":" ++ toString i.owner ++ ",\"seal_hash\":" ++ jsonOptionalNat i.sealHash ++ "}"

def explicitJson (e : Explicit) : String :=
  "{\"workspace_id\":" ++ jsonOptionalNat e.workspaceId ++ ",\"seal_hash\":" ++ jsonOptionalNat e.sealHash ++
  ",\"authority\":" ++ jsonOptionalString (e.authority.map BindingAuthority.toDefraDB) ++ "}"

def rootJson (r : Root) : String :=
  "{\"doc_id\":" ++ toString r.docId ++ ",\"correlation\":" ++ toString r.correlation ++
  ",\"revision\":" ++ toString r.revision ++ ",\"entry_route\":" ++ toString r.entryRoute ++
  ",\"authenticated_target\":" ++ boolString r.authenticatedTarget ++
  ",\"well_formed_tuple\":" ++ boolString r.wellFormedTuple ++
  ",\"workspace\":" ++ optionalIdentityJson r.workspace ++ "}"

def sourceJson : Source → String
  | .downstream roots => "{\"kind\":\"downstream\",\"roots\":" ++ jsonArray (roots.map rootJson) ++ "}"
  | .bootstrap input stamped seed owner => "{\"kind\":\"bootstrap\",\"controller_input\":" ++
      explicitJson input ++ ",\"stamped\":" ++ optionalIdentityJson stamped ++
      ",\"physical_seed_verified\":" ++ boolString seed ++
      ",\"workspace_owner_verified\":" ++ boolString owner ++ "}"

def caseJson (c : Case) : String :=
  "{\"name\":" ++ jsonString c.name ++
  ",\"context\":{\"correlation\":" ++ toString c.ctx.correlation ++
  ",\"revision\":" ++ toString c.ctx.revision ++ ",\"entry_route\":" ++ toString c.ctx.entryRoute ++
  ",\"run_and_plan_verified\":" ++ boolString c.ctx.runAndPlanVerified ++
  ",\"destination_route_verified\":" ++ boolString c.ctx.destinationRouteVerified ++
  ",\"destination_authority\":" ++ jsonOptionalString (c.ctx.destinationAuthority.map BindingAuthority.toDefraDB) ++ "}" ++
  ",\"source\":" ++ sourceJson c.source ++ ",\"explicit\":" ++ explicitJson c.explicit ++
  ",\"expected\":" ++ (match c.expected with
    | none => "null"
    | some r => "{\"workspace\":" ++ optionalIdentityJson r.workspace ++
        ",\"authority\":" ++ jsonOptionalString (r.authority.map BindingAuthority.toDefraDB) ++ "}") ++
  ",\"generation\":" ++ toString c.before.graph.generation ++
  ",\"expected_generation\":" ++ toString c.expectedGeneration ++
  ",\"cancelled\":" ++ boolString c.before.graph.run.cancellationRequested ++
  ",\"primary_cause\":" ++ jsonOptionalNat c.before.graph.primary ++
  ",\"published\":" ++ boolString c.published ++ "}"

def casesJson : String := jsonArray (cases.map caseJson)
namespace SessionCases
open GraphPipeline.WorkspaceLineage.SessionContinuation

structure SessionCase where
  name : String
  eligibility : Eligibility := ⟨true,true,true,1,.selectedEntry⟩
  ctx : Context := context
  owner : Nat := 21
  firingNode : Nat := 9
  targetNode : Nat := 3
  candidates : List Candidate := [⟨root,21,51⟩]
  deriving DecidableEq, Repr

def cases : List SessionCase :=
  [ {name := "selected_entry_parallel_is_singleton"}
  , {name := "unique_group_parallel_is_singleton", eligibility := ⟨true,true,true,1,.grouped⟩}
  , {name := "per_document_serial_is_not_singleton", eligibility := ⟨true,true,true,1,.perDocument⟩}
  , {name := "multiple_routes_are_not_singleton", eligibility := ⟨true,true,true,2,.grouped⟩}
  , {name := "target_missing", eligibility := ⟨true,false,true,1,.selectedEntry⟩}
  , {name := "target_is_plugin", eligibility := ⟨true,true,false,1,.selectedEntry⟩}
  , {name := "source_is_plugin", eligibility := ⟨false,true,true,1,.selectedEntry⟩}
  , {name := "self_continue_rejected", targetNode := 9}
  , {name := "root_missing", candidates := []}
  , {name := "two_authentic_roots_rejected", candidates := [⟨root,21,51⟩,⟨{root with docId := 42},21,52⟩]}
  , {name := "foreign_owner_rejected", candidates := [⟨root,99,51⟩]}
  , {name := "unsigned_root_rejected", candidates := [⟨{root with authenticatedTarget := false},21,51⟩]}
  , {name := "unsigned_lookalike_ignored", candidates := [⟨root,21,51⟩,⟨{root with authenticatedTarget := false},21,52⟩]}
  , {name := "wrong_correlation_rejected", candidates := [⟨{root with correlation := 99},21,51⟩]}
  , {name := "wrong_revision_rejected", candidates := [⟨{root with revision := 99},21,51⟩]}
  , {name := "wrong_target_route_rejected", candidates := [⟨{root with entryRoute := 99},21,51⟩]}
  , {name := "unverified_run_rejected", ctx := {context with runAndPlanVerified := false}}
  , {name := "unverified_destination_rejected", ctx := {context with destinationRouteVerified := false}}
  ]

private def object (fields : List (String × String)) : String :=
  "{" ++ String.intercalate "," (fields.map fun (key,value) => jsonString key ++ ":" ++ value) ++ "}"
private def routeName : RouteKind → String
  | .selectedEntry => "selected_entry" | .grouped => "grouped" | .perDocument => "per_document"
private def candidateJson (c : Candidate) : String := object [
  ("root", rootJson c.root), ("owner", toString c.owner), ("session", toString c.session)]
private def selectionJson : Option Selection → String
  | none => "null"
  | some selected => object [("session",toString selected.session), ("root_doc",toString selected.rootDoc),
      ("firing_node",toString selected.firingNode)]
private def caseJson (c : SessionCase) : String := object [
  ("name",jsonString c.name),
  ("eligibility", object [("source_is_task",boolString c.eligibility.sourceIsTask),
    ("target_exists",boolString c.eligibility.targetExists), ("target_is_task",boolString c.eligibility.targetIsTask),
    ("route_count",toString c.eligibility.routeCount), ("route_kind",jsonString (routeName c.eligibility.routeKind))]),
  ("context", object [("correlation",toString c.ctx.correlation), ("revision",toString c.ctx.revision),
    ("target_route",toString c.ctx.entryRoute), ("run_and_plan_verified",boolString c.ctx.runAndPlanVerified),
    ("destination_route_verified",boolString c.ctx.destinationRouteVerified)]),
  ("owner",toString c.owner), ("firing_node",toString c.firingNode), ("target_node",toString c.targetNode),
  ("candidates",jsonArray (c.candidates.map candidateJson)), ("eligible",boolString (eligible c.eligibility)),
  ("expected",selectionJson (SessionContinuation.resolve c.eligibility c.ctx c.owner c.firingNode c.targetNode c.candidates))]

def casesJson : String := jsonArray (cases.map caseJson)
example : SessionContinuation.resolve ⟨true,true,true,1,.selectedEntry⟩ context 21 9 3 [⟨root,21,51⟩] =
    some ⟨51,41,9⟩ := by decide
example : SessionContinuation.resolve ⟨true,true,true,1,.grouped⟩ context 21 9 3 [⟨root,21,51⟩] =
    some ⟨51,41,9⟩ := by decide
end SessionCases

end Conformance.GraphWorkspaceLineageContracts
