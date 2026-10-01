use anyhow::{bail, Result};

pub(super) fn page(path: &[String]) -> Result<&'static str> {
    let words: Vec<_> = path.iter().map(String::as_str).collect();
    match words.as_slice() {
        []=>Ok("schema RESOURCE VERB; command words go in argv, collection names/version IDs in target_id, inputs in options.\ncollection: list, get, create, update, materialize\nversion: list, get, activate\nmigration: set\nview: create\nbatch: ordered calls in options.operations; each commits separately, stops on error, retains earlier results\nReads need no preview. For a mutation, set preview:true, inspect its effect, then apply the returned next_call. Digests bind inputs and observed schemas; re-preview after changes. Gents-managed schemas evolve through product releases. This tool changes definitions, not document permissions. Help: [\"help\",RESOURCE]."),
        ["batch"] => Ok("batch takes options.operations: 1–64 ordinary schema calls. Each mutation carries its own preview or digest. Calls run in order; the first error stops the batch, earlier changes remain, and later calls are not attempted. Nested batches are rejected. Preview dependent operations after their dependencies commit."),
        ["collection"] | ["collection","list"|"get"|"create"|"update"|"materialize"]=>Ok(r#"collection list: no target. get: target_id is the collection name; returns DefraDB's field types, version and metadata.
create: options.sdl is GraphQL SDL, e.g. "type WorkItem { handoff_id: String, title: String }". Scalars include String, Int, Float, Boolean and DateTime. Creates new collections; an existing matching definition is a no-op. To change existing fields, use update.
update: target_id is a collection name; options.patch is an RFC 6902 array using DefraDB's field names and /COLLECTION/... paths. Add a nullable field: {"argv":["collection","update"],"target_id":"WorkItem","options":{"patch":[{"op":"add","path":"/WorkItem/Fields/-","value":{"Name":"handoff_id","Kind":"String"}}]},"preview":true}. Inspect get before using field indexes. DefraDB validates allowed changes, indexes and relationships; it may create a new version. A nullable field addition needs no lens; old rows have no supplied value. Update writers separately through config. Never fabricate historical values.
materialize: target_id is a collection name; advance cached documents through registered migrations to the active version. This does not create document commits or synthesize missing business values.
Schema mutations are separate from config-document transactions. Node-wide definitions may serve several agents. Collection deletion is not exposed by the pinned native adapter."#),
        ["version"] | ["version","list"|"get"|"activate"]=>Ok("version list: optional target_id filters by collection name; includes active and inactive definitions. get: target_id is an exact VersionID from list. activate: preview:true and target_id select the version; applying makes it active and deactivates siblings. Inspect its fields and migration path first. Activation is not a data rollback; existing documents may require transformations. For a transforming change: patch with /COLLECTION/IsActive=false, register the migration between the source and new version, then activate and materialize."),
        ["migration"] | ["migration","set"]=>Ok(r#"migration set: options.config is DefraDB LensConfig: {"SourceCollectionVersionID":"SOURCE","DestinationCollectionVersionID":"DESTINATION","Lenses":[{"Module":"BASE64_WASM","Arguments":{},"Inverse":false}]}. Discover exact version IDs with version list. Versions must exist and be adjacent as required by DefraDB. Modules implement the DefraDB lens WASM contract; this tool registers compiled modules, it does not compile them. Inline Module bytes only: Path is rejected because schema access does not grant file access. A lens transforms existing documents between versions; additive nullable fields usually need no lens. Preview then apply; DefraDB validates the module and migration. Inspect version get for the saved transform metadata, then activate the destination and materialize."#),
        ["view"] | ["view","create"]=>Ok(r#"view create: options.query is a source selection such as "WorkItem { title }"; options.sdl declares the target view collection. Preview then apply. DefraDB validates the source query and target schema. Inspect the installed definition using collection get. Views do not grant access to their source documents."#),
        _=>bail!("unknown schema help path; use [\"help\"]"),
    }
}

pub(super) fn effect(words: &[&str]) -> &'static str {
    match words {
        ["collection","create"]=>"Install new collection definitions; matching definitions are retained.",
        ["collection","update"]=>"Patch the collection definition. DefraDB may create and activate a new version; removing or changing fields can affect readers and writers. Existing data values are not backfilled.",
        ["version","activate"]=>"Switch the active version and reindex through registered migrations; this is not a document rollback.",
        ["migration","set"]=>"Register the inline WASM transformation between the specified versions.",
        ["view","create"]=>"Create a view from the source selection and target schema.",
        ["collection","materialize"]=>"Migrate and cache known documents at the active version, without new document commits.",
        _=>"Inspect the command help.",
    }
}
