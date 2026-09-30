# Configure the factory crew

Configure the crew for the large refactor in `factory/crew_spec.json`. Stop after setup. I will launch the crew and run acceptance myself.

Read these files under your tool root:
- `factory/crew_spec.json`: exact IDs, profiles, limits, roles, grants, collections, surfaces, targets and automation.
- `factory/crew_prompts.md`: each role's instructions and your Run mode block.
- `factory/task_templates.md`: each Task's input template.

Reuse the existing backend and leave its concurrency unchanged. Save each role's grants as specified. Your own tools are for configuration and reading these files.

A behavior ID is `<DID>:<slug of its display name>`; other IDs are stored as written. New tool selections take effect on the next request. Complete setup through the config tool; no extra automation is needed to continue this conversation.

Done means every specified record exists, its references resolve, and each behavior selects the right context, tools and profile. Check the stored configuration, including field contracts, surface selection, templates, routing and outcome settings. Add the Run mode block to your Context.

File one mailbox item titled "Factory setup receipt" listing the configured IDs, grants and any unresolved requirements. Say that execution acceptance is pending. Do not launch crew sessions, seed work, run smoke checks or begin the refactor.
