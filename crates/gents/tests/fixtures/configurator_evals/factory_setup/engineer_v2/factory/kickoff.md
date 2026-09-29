# Kickoff: factory crew setup (#1786)

Build the crew for run #1786 on this fresh node and prove it works. Do not start the run. I am not watching; the mailbox is the only channel to me.

## Inputs

Read these with `read_file` before you configure anything. Paths are relative to your tool root.

- `factory/crew_spec.json`: the crew, declaratively: profiles, executions, agents (role, profile, prompt, tool grants, surface), targets, collections and fields, handoff IDs and automation rows. Its IDs are exact.
- `factory/crew_prompts.md`: your run mode and every crew prompt, by section.
- `factory/task_templates.md`: every Task template, by task ID.

## Rules

- Use the native `config` tool for every change. Take command syntax from `config` help (`["help"]`, `["help", RESOURCE]`); never guess a command or field.
- Every step is **create → read back → expect**: preview, apply, read the document back, and compare it with the spec entry. Fix a mismatch before you go on. After an error, reread state and correct that operation; never duplicate completed work.
- This node has file tools under your tool root and no bash, git or GitHub. Save each agent's grants as the spec says; the process ceiling narrows them, so report the effective authority. Where your run mode says to file a GitHub issue, send a mailbox item with the exact command and error instead.
- One backend exists. Do not edit it or its `max_concurrent`.
- **IDs.** Profile, execution, target, surface, task, trigger and schedule IDs are stored as given. A behavior you create comes back as `<DID>:<id>`: use that exact returned ID wherever a behavior is named (`options.behavior`, a target's `behavior_id`, a Task's behavior).
- **Nested groups.** `set` replaces a whole field. Before you change `host`, `datastore` or `subagents` on any Tools (your own included), read it and send the full group with your change, so `engineer-mailbox` and read-only query stay on yours.
- **Filters.** An event source `filter` is a string holding a GraphQL object literal with unquoted keys, exactly as the spec writes it: `"{ k: { _eq: 0 }, lead: { _eq: \"impl\" } }"`.
- Tag every test document `test`. Create no `RunStart` without that tag, and leave the `engineer-watch` trigger disabled.
- Test results arrive in this session as new messages. When you have started a test and have nothing else to do, end your turn. The node shuts down once it has been quiet for a minute, so send the receipt as soon as step 9 is done.

## Procedure

1. **Run mode.** Append the Run mode block of `crew_prompts.md` to your own Context's system prompt. There is no append: read your Context, then set `system_prompt` to the shipped prompt, a blank line and the block. Expect: the read-back begins with the shipped prompt and ends with that block.
2. **Preflight.** Expect: `config` help lists `schema`, `datastore`, `subagent-target`, `execution` and `automation`; your Tools carry `self_config_preview`, the agents tools, the sessions tool, read-only query and `engineer-mailbox`; `backend list` shows the backend's catalog with `GLM-5.3-Flash-NVFP4`. Report your tool root and the process ceiling.
3. **Profiles and executions.** For each `profiles` entry, create its execution, then the profile on the backend with `model_name` and `execution_id`. Set the lead executions' limits as `executions` says. Expect: each profile reads back with that backend, model and execution.
4. **Collections.** Install one schema per `collections` entry: the common fields plus its own, typed as `rules` says. Expect: `schema get` shows exactly those fields for each.
5. **Agents.** For each `agents` entry: create the behavior with its own Context and Tools on its profile; its system prompt is the `crew_prompts.md` sections named in `prompt`, in order, verbatim. Set its Tools (`files`, `bash`, `agents_tools`, root). Its surface is four separate checks: the schemas from step 4; `datastore create` with one create entry per `creates` collection and one query entry (exact match on `handoff_id`) per `queries` collection, each entry with a `description`; the surface ID added to that agent's `datastore.datastore_tool_surface_ids`; and, in step 9, a call from a fresh session of the agent. Do the same for your own `engineer` entry, keeping `engineer-mailbox`. Expect: `behavior get` shows the profile, the prompt and the grants in `runtime_effective`.
6. **Targets.** Create each `targets` entry and select it in each `selected_by` agent's Tools (`subagents {target_ids, enabled: true}`, keeping existing IDs). Expect: the leads and you show `agent_new`; nobody else has the agents tools.
7. **Automation.** For each `automation` row, as `automation_rules` says: the event source, the Task (template from `task_templates.md`), then the trigger. Expect: each Task and trigger reads back with the row's behavior, session template, concurrency, `emit_outcome` and enabled state.
8. **Field check.** For every Task, every `{{ doc.* }}` in its template must be a field of its row's source collection. Report the check.
9. **Prove it.** All documents tagged `test`; run IDs `impl-test-1` and `impl-test-2`; your own session is `reply_session_id` on the `RunStart`s.
   1. Smoke: create two `RunStart`s (`k = 0`, `lead = impl`) at once. Each test lead runs `ShardAssignment` → worker → `ShardResult` → reviewer → `ShardReview` on both workstation routes. Confirm each review reaches the session that created its assignment, and that two reviews created back to back into one busy session arrive in order. One assignment tells its worker to finish without a `ShardResult`: confirm its `FireOutcome` arrives and the lead creates `attempt + 1`. Run `BuildRequest` → `BuildResult` (commands `cargo --version`, `lake --version`; the Verifier has no bash here, so it reports `failed` with the exact error) and `GateRequest` → `GateResult` (search `repo/` for `persona`).
   2. Load: 8 `audit` assignments at once, 4 per workstation, each counting `agent_did` hits in one of `repo/audit/d1` … `repo/audit/d8`, with a test lead's session as `reply_session_id`. Expect: every assignment ends in a result and an outcome, and every result reaches that session.
   3. Protocol: one processed outcome creates no further outcome; a lead request that completes while its Goal continues produces no lead outcome; a `blocked` result reaches Inbox Blocked, not a reviewer; a continuation `RunStart` (`k = 1`, `target_session_id` = a test lead's session) resumes that session while one of its workers runs, and that worker's result arrives there.
   4. Steering: find a running worker session by the `session_id` in its documents, message it, and read its reply with the sessions tool.
   Record every request, document and fire ID. Report runtime defects with that evidence and work around them through configuration.
10. **Receipt.** Stop the test lead sessions with a message, then send the setup receipt your run mode describes as a mailbox item: every ID you created, grants per agent, the backend concurrency, the test results with request IDs, and anything untested or degraded.
