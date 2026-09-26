# Prompt proposer

The prompt proposer is the model side of `gents optimization run`. It is a
single behavior, `prompt-proposer`, with a fixed system prompt and no tools:
it cannot read or write documents, files or configuration. Each round it
receives one behavior's current system instruction and the feedback a
training run produced against it, and answers with one rewritten
instruction and the rationale for it. The optimization driver, never the
proposer, decides whether the candidate is worth a validation run.

## Configuration

The pack declares one inference slot, `proposer`, bound to the
`prompt-proposer` behavior. `gents optimization run` installs the pack into
the operator's home once, idempotently, and binds that slot to
`--proposer-profile` or the home's default profile. Nothing else is
configurable: the behavior's context is the proposing contract in
`agent_behaviors/prompt_proposer/system_prompt.md` (what the turn carries,
what to carry over from the feedback, the rules a candidate is held to, and
the reply format), and its tools document grants no host bash and no file,
datastore or self-config tools.

## Usage

```
gents optimization run <definition_id> --subject <pack>[:<behavior>] --proposer behavior:prompt_proposer[:<behavior>] [--proposer-profile <profile_id>]
```

The command opens a fresh session against the served home and sends the
proposer one user turn per round: the current instruction, every feedback
line from the train run, the candidates already rejected, the rules, and the
reply shape. A reply that contains exactly one fenced `json` block holding
`text` and `rationale` is the round's proposal; a reply that does not gets
one corrective turn, and a second failure fails the run; rerun with the
same `--job-id` to resume. The driver's
structural gate, not the proposer, enforces the byte cap and the
no-repeat rule. `--proposer scripted:<file>` replays a fixed list of
proposals instead and installs nothing.
