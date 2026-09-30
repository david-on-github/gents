# Factory crew setup (#1786), in checkpoints

## Objective

Build the crew in `factory/crew_spec.json` on this node and prove that its handoffs work. Do not start the run. I am not watching; the mailbox is the only channel to me.

These paths are relative to your tool root:
- `factory/crew_spec.json`: the crew, with exact IDs.
- `factory/crew_prompts.md`: your Run mode block and the crew prompts, by section.
- `factory/task_templates.md`: the Task templates, by task ID.

## Facts about this node

- One backend serves `GLM-5.3-Flash-NVFP4`. Leave it and its `max_concurrent` unchanged.
- The process ceiling allows file tools under your tool root and no bash, git or GitHub. Save each agent's grants as the spec states; the ceiling narrows them at run time.
- A behavior's ID is `<DID>:<slug of its display name>`. The spec's display names slug to its IDs. Every other ID is stored exactly as given.
- `profile` and `tools` commands name their behavior (`options.behavior`). A trigger can reference only a Task of the behavior it is written under.
- An event source `filter` is a string holding a GraphQL object literal, exactly as the spec writes it.
- Tools you add to your own Tools take effect in your next request, not the current one. This session gets a next request only when automation delivers into it. You can arrange one with an EventSource on `MailboxItem` filtered on a title of your choice, a Task of your own behavior, and a trigger whose `session_id_template` is your session ID; filing that mailbox item then wakes you.

## Checkpoints

Finish each checkpoint before you start the next. A checkpoint is done when every read-back matches the spec. Fix any mismatch where it occurs.

1. **Run mode.** Your Context ends with the Run mode block and still begins with the shipped prompt.
2. **Inference.** Every profile reads back on the backend with its model and its execution, and the lead executions carry the limits the spec states.
3. **Agents.** Every agent's `behavior get` shows its profile, its prompt (its own section, then Crew rules) and its grants in `runtime_effective`. The leads and you hold the agents tools, with the targets the spec lists. Nobody else holds them.
4. **Data.** `schema get` shows each collection with exactly its fields. Each agent's surface exists with a create entry per `creates` collection and an exact-match query entry per `queries` collection, and the agent's Tools select it. Your Tools still select `engineer-mailbox` and keep read-only query.
5. **Automation.** Every automation row's Task and trigger read back with the row's behavior, template, session template, concurrency, `emit_outcome` and enabled state. Every `{{ doc.* }}` in a template is a field of that row's source collection.
6. **Continuation.** Your wake route exists, and your next request carries your new surface's tools.
7. **Proof.** Every test document is tagged `test`, the run IDs are `impl-test-1` and `impl-test-2`, and your session is the `RunStart`s' `reply_session_id`.
   - Two Impl Lead smoke runs each take an assignment through worker → result → reviewer → review on both workstation routes, and each review arrives in the session that created its assignment.
   - An assignment whose worker ends without a result yields a `FireOutcome` and an attempt 2.
   - A `BuildRequest` gets a `BuildResult`: the Verifier has no bash here, so it reports `failed` with the exact error. A `GateRequest` gets a `GateResult`.
   - Eight audit assignments, one per `repo/audit/d1` … `d8` and four per workstation, each end in a result and an outcome that reach a test lead's session.
8. **Receipt.** A mailbox item names every ID you created, the effective grants per agent, the test request IDs, and anything untested.
