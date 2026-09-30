# factory_setup_eval

The `factory-setup` eval definition: one Engineer, alone on a fresh trial node,
builds the #1786 factory crew from a single kickoff and proves its handoffs,
setup only. Run it over one subject pack per kickoff variant:

```
gents pack install <this directory> --home HOME
gents eval run factory-setup --home HOME --trials 1 \
  --cell v1=<factory_setup>/engineer_v1 --profile v1=PROFILE \
  --cell v2=<factory_setup>/engineer_v2 --profile v2=PROFILE
```

`handoff_delivery` is the harness gate (a violation is infrastructure, a Gents
finding); `crew_spec_match` and the other checks grade the model.
