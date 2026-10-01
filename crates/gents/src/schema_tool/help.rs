use anyhow::{bail, Result};

pub(super) fn page(path: &[String]) -> Result<&'static str> {
    let words: Vec<_> = path.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => Ok("schema RESOURCE VERB; IDs follow the verb or use target_id, inputs go in options.\ncollection: list, get, create, update, materialize\nversion: list, get, activate\nmigration: set\nview: create\nbatch: ordered calls in options.operations\nMutations require RESOURCE preview VERB first; inspect the effect, then use the returned next_call with options.digest. Changed inputs or schemas require a new preview. Reads need neither. Gents-managed schemas evolve through product releases. This tool changes definitions, not document permissions. Help: RESOURCE VERB --help."),
        ["batch"] => Ok("batch takes options.operations: 1–64 schema calls. Calls run in order; the first error stops the batch, earlier changes remain, and later calls are not attempted. Nested batches are rejected. Each mutation needs its own preview result. Preview dependent operations after their dependencies commit."),
        ["collection"] => Ok("collection defines stored document fields. list and get inspect definitions; create installs new definitions; update patches existing ones; materialize advances cached documents through registered migrations. Use collection VERB --help for inputs. Collection deletion is not exposed by the pinned native adapter."),
        ["collection", "list"] => Ok("collection list: no ID or options. Returns collection names; inspect one with collection get NAME."),
        ["collection", "get"] => Ok("collection get NAME: returns DefraDB field types, version and metadata. NAME may instead use target_id. No options."),
        ["collection", "create"] => Ok(r#"collection preview create: options.sdl is GraphQL SDL; collection names come from it, so omit target_id. Example: {"argv":["collection","preview","create"],"options":{"sdl":"type WorkItem { handoff_id: String, title: String }"}}. Inspect the effect, then use next_call to create with options.digest. Scalars include String, Int, Float, Boolean and DateTime. An existing matching definition is a no-op; use collection update to change one."#),
        ["collection", "update"] => Ok(r#"collection preview update NAME: options.patch is an RFC 6902 array using DefraDB's field names and /COLLECTION/... paths. Example: {"argv":["collection","preview","update","WorkItem"],"options":{"patch":[{"op":"add","path":"/WorkItem/Fields/-","value":{"Name":"handoff_id","Kind":"String"}}]}}. Inspect get before using field indexes, then review the preview and use next_call. DefraDB validates changes and may create a new version. A nullable field addition needs no lens; old rows have no supplied value. Update writers separately through config; never fabricate historical values."#),
        ["collection", "materialize"] => Ok("collection preview materialize NAME: no options. Review the preview and use next_call to advance cached documents through registered migrations to the active version. This creates no document commits and synthesizes no business values."),
        ["version"] => Ok("version inspects published collection definitions. list discovers exact VersionIDs; get inspects one; activate makes one active. Use version VERB --help for inputs."),
        ["version", "list"] => Ok("version list [NAME]: optionally filter by collection name, either after list or in target_id. Includes active and inactive definitions and exact VersionIDs. No options."),
        ["version", "get"] => Ok("version get VERSION_ID: copy an exact VersionID from version list. It may instead use target_id. Returns definition and transform metadata. No options."),
        ["version", "activate"] => Ok("version preview activate VERSION_ID: no options. Review the effect, then use next_call to activate this version and deactivate siblings. Activation is not a data rollback. For a transforming change: patch with /COLLECTION/IsActive=false, register the migration between source and new version, then activate and materialize."),
        ["migration"] => Ok("migration set registers a compiled WASM lens between collection versions. Use migration set --help for the LensConfig shape and version workflow."),
        ["migration", "set"] => Ok(r#"migration preview set: options.config is DefraDB LensConfig: {"SourceCollectionVersionID":"SOURCE","DestinationCollectionVersionID":"DESTINATION","Lenses":[{"Module":"BASE64_WASM","Arguments":{},"Inverse":false}]}. Omit target_id. Discover exact version IDs with version list. Versions must exist and be adjacent as required by DefraDB. Modules implement the DefraDB lens WASM contract; this tool registers compiled modules, not source code. Inline Module bytes only: Path is rejected because schema access grants no file access. Review the preview and use next_call; DefraDB validates publication. Inspect version get, then activate and materialize. Additive nullable fields usually need no lens."#),
        ["view"] => Ok("view create defines a query-backed collection. Use view create --help for its query and SDL inputs."),
        ["view", "create"] => Ok("view preview create: options.query is a source selection such as WorkItem { title }; options.sdl declares the target view collection. Omit target_id. Review the preview and use next_call. DefraDB validates the query and schema. Inspect the result with collection get. Views grant no access to source documents."),
        _ => bail!("unknown schema help path; use [\"help\"]"),
    }
}

pub(super) fn effect(words: &[&str]) -> &'static str {
    match words {
        ["collection", "create"] => "Install new collection definitions; matching definitions are retained.",
        ["collection", "update"] => "Patch the collection definition. DefraDB may create and activate a new version; removing or changing fields can affect readers and writers. Existing data values are not backfilled.",
        ["version", "activate"] => "Switch the active version and reindex through registered migrations; this is not a document rollback.",
        ["migration", "set"] => "Register the inline WASM transformation between the specified versions.",
        ["view", "create"] => "Create a view from the source selection and target schema.",
        ["collection", "materialize"] => "Migrate and cache known documents at the active version, without new document commits.",
        _ => "Inspect the command help.",
    }
}
