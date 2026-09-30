# Factory crew setup (#1786)

Build the crew described in `factory/crew_spec.json` on this node and prove that it works. Do not start the run. Its prompts are in `factory/crew_prompts.md` and its Task templates are in `factory/task_templates.md`; paths are relative to your tool root. I am not watching; the mailbox is the only channel to me.

Done means all of the following hold:

- Every entry in the spec exists with its exact ID, fields and grants. Your own Context ends with the spec's Run mode block.
- The tests pass. Every test document is tagged `test`, and the test run IDs are `impl-test-1` and `impl-test-2`.
  - Two Impl Lead smoke runs each take an assignment through worker → result → reviewer → review, on both workstation routes, and each review arrives in the session that created its assignment.
  - An assignment whose worker ends without a result yields a `FireOutcome` and an attempt 2.
  - A `BuildRequest` gets a `BuildResult`, and a `GateRequest` gets a `GateResult`.
  - Eight audit assignments, one per directory `repo/audit/d1` … `repo/audit/d8` and four per workstation, each end in a result and an outcome that reach a test lead's session.
- A setup receipt in the mailbox names every ID you created and the test request IDs.
