# Factory crew setup (#1786)

## Objective

Build the crew in `factory/crew_spec.json` on this node and prove that its handoffs work. Do not start the run. I am not watching; the mailbox is the only channel to me.

## Inputs

These paths are relative to your tool root.

- `factory/crew_spec.json`: the crew. It lists profiles, executions, agents (role, profile, prompt, grants and surface), targets, collections and fields, handoff IDs and automation rows. Its IDs are exact.
- `factory/crew_prompts.md`: your Run mode block and every crew prompt, by section.
- `factory/task_templates.md`: every Task template, by task ID.

## Facts about this node

- One backend serves `GLM-5.3-Flash-NVFP4`. Leave it and its `max_concurrent` unchanged.
- The process ceiling allows file tools under your tool root and no bash, git or GitHub. Save each agent's grants as the spec states; the ceiling narrows them at run time.
- A behavior's ID is `<DID>:<slug of its display name>`. The spec's display names slug to its IDs. Every other ID is stored exactly as given.
- `profile` and `tools` commands name their behavior (`options.behavior`). A trigger can reference only a Task of the behavior it is written under.
- An event source `filter` is a string holding a GraphQL object literal, exactly as the spec writes it.
- Tools you add to your own Tools take effect in your next request, not the current one. This session gets a next request only when automation delivers into it. You can arrange one with an EventSource on `MailboxItem` filtered on a title of your choice, a Task of your own behavior, and a trigger whose `session_id_template` is your session ID; filing that mailbox item then wakes you.

## Done means

- Every spec entry exists and reads back as the spec states: IDs, profiles and executions, prompts, grants, surfaces and their selection, targets, collections and fields, Tasks, triggers and `emit_outcome`. Your own Context ends with the Run mode block.
- The tests pass. Every test document is tagged `test`, the run IDs are `impl-test-1` and `impl-test-2`, and your session is the `RunStart`s' `reply_session_id`.
  - Two Impl Lead smoke runs each take an assignment through worker → result → reviewer → review on both workstation routes. Each review arrives in the session that created its assignment, and two reviews created back to back into one busy session arrive in order.
  - An assignment whose worker ends without a result yields a `FireOutcome` and an attempt 2.
  - A `BuildRequest` gets a `BuildResult`. The Verifier has no bash here, so it reports `failed` with the exact error. A `GateRequest` gets a `GateResult` from a search of `repo/` for `persona`.
  - Eight audit assignments, one per directory `repo/audit/d1` … `repo/audit/d8` and four per workstation, count `agent_did` hits. Each ends in a result and an outcome that reach a test lead's session.
  - A blocked result reaches Inbox Blocked, not a reviewer.
  - A continuation `RunStart` (`k = 1`) resumes a test lead's session.
- A setup receipt in the mailbox names every ID you created, the effective grants per agent, the test request IDs, and anything untested.
