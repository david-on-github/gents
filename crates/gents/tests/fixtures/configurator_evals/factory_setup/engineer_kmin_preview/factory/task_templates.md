# Task templates

Copy each template verbatim into the named Task field. Each renders only fields of its source collection (crew_spec.json `collections`, plus FireOutcome's runtime fields). In engineer-watch, replace CONSOLE_SESSION_ID with your own session ID (the sessions tool marks it `is_current`).

## lead-proofs, lead-impl

Field: `goal_objective_template`

```
Run {{ doc.run_id }}: {{ doc.objective }} Your session_id is {{ session.session_id }}; use it as reply_session_id on every document you create. Report outcomes to {{ doc.reply_session_id }}. Complete this Goal only when your prompt's completion condition holds.
```

## work-a, work-b

Field: `prompt_template`

```
Shard {{ doc.shard_id }} (stage {{ doc.stage }}, phase {{ doc.phase }}, attempt {{ doc.attempt }}, handoff {{ doc.handoff_id }}). Worktree: {{ doc.worktree }}. Owned files: {{ doc.owned_files }}. Base: {{ doc.base_sha }}. Instructions: {{ doc.instructions }}. Worklist: {{ doc.worklist }}. Review notes: {{ doc.review_notes }}. Reviewer: {{ doc.reviewer }}. Run: {{ doc.run_id }}. Reply session: {{ doc.reply_session_id }}.
```

## review-claude, review-astra, review-grok

Field: `prompt_template`

```
Review {{ doc.handoff_id }} (shard {{ doc.shard_id }}, stage {{ doc.stage }}, phase {{ doc.phase }}, attempt {{ doc.attempt }}) in {{ doc.worktree }}. Owned files: {{ doc.owned_files }}. Assignment: {{ doc.assignment_id }}. Worker status: {{ doc.status }}. Worker report: {{ doc.report }}. Run: {{ doc.run_id }}. Reply session: {{ doc.reply_session_id }}.
```

## build

Field: `prompt_template`

```
Build {{ doc.build_id }} ({{ doc.handoff_id }}): stage {{ doc.stage }}, commit {{ doc.commit }}. Commands, in order: {{ doc.commands }}. Run: {{ doc.run_id }}. Reply session: {{ doc.reply_session_id }}.
```

## gate

Field: `prompt_template`

```
Gate {{ doc.gate_id }} ({{ doc.handoff_id }}): stage {{ doc.stage }}, commit {{ doc.commit }}, scope {{ doc.scope }}. Queries: {{ doc.queries }}. Kept: {{ doc.kept }}. Downstream: {{ doc.downstream }}. Run: {{ doc.run_id }}. Reply session: {{ doc.reply_session_id }}.
```

## inbox-audit, inbox-audit-proofs

Field: `prompt_template`

```
Audit result {{ doc.handoff_id }} for shard {{ doc.shard_id }} (stage {{ doc.stage }}): status {{ doc.status }}. Worklist and counts: {{ doc.report }}
```

## inbox-blocked, inbox-blocked-proofs

Field: `prompt_template`

```
Blocked result {{ doc.handoff_id }} for shard {{ doc.shard_id }} attempt {{ doc.attempt }} (stage {{ doc.stage }}, phase {{ doc.phase }}). Report: {{ doc.report }}
```

## inbox-review, inbox-review-proofs

Field: `prompt_template`

```
Review {{ doc.handoff_id }} for shard {{ doc.shard_id }} attempt {{ doc.attempt }} (stage {{ doc.stage }}): {{ doc.verdict }}. Notes: {{ doc.review_notes }}. Files: {{ doc.owned_files }}.
```

## inbox-build, inbox-build-proofs

Field: `prompt_template`

```
Build {{ doc.build_id }} at {{ doc.commit }} (stage {{ doc.stage }}): {{ doc.status }}. Failures: {{ doc.failures }}
```

## inbox-gate, inbox-gate-proofs

Field: `prompt_template`

```
Gate {{ doc.gate_id }} at {{ doc.commit }} (stage {{ doc.stage }}): {{ doc.status }}. Hits: {{ doc.hits }}
```

## inbox-outcome, inbox-outcome-proofs, inbox-outcome-engineer

Field: `prompt_template`

```
Fire outcome {{ doc.handoff_id }} for {{ doc.source_handoff_id }}: {{ doc.terminal_state }}. Reason: {{ doc.reason }}. Session: {{ doc.session_id }}. Request: {{ doc.request_id }}.
```

## engineer-watch

Field: `prompt_template`

```
Supervision pass for the node/agent refactor run. Console session: CONSOLE_SESSION_ID. Do one supervision pass as your run mode says (continuations, overdue gaps, configuration fixes, mailbox items), reply with a short summary, and end. Never start a second watch.
```
