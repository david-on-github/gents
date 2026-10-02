//! The config tool's model-facing contract: short behavior IDs, receipts that
//! name their target, refused silent Tools drops, the validation gaps the
//! factory-setup audit found, and help recipes that run as written.

use super::tests::{build_persona_node, call_config_tool, config, persona_identity};
use super::*;

type Tools = Vec<Box<dyn ToolDyn>>;

async fn call(tools: &Tools, args: Value) -> Result<Value, String> {
    let tool = tools
        .iter()
        .find(|tool| tool.name() == CONFIG_TOOL_NAME)
        .expect("config registered");
    match tool.call(args.to_string()).await {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))),
        Err(error) => Err(format!("{error:#}")),
    }
}

async fn ok(tools: &Tools, args: Value) -> Value {
    call(tools, args.clone())
        .await
        .unwrap_or_else(|error| panic!("{args}: {error}"))
}

async fn refused(tools: &Tools, args: Value) -> String {
    match call(tools, args.clone()).await {
        Ok(value) => panic!("{args} was accepted: {value}"),
        Err(error) => error,
    }
}

async fn setup(label: &str, categories: &[&str]) -> (Arc<EmbeddedNode>, String, Tools) {
    let node = build_persona_node().await;
    let identity = persona_identity(label);
    let owner = identity.did().to_string();
    crate::test_support::install_test_behavior(&node, &owner, "beh-test").await;
    let mut grants = config(categories);
    grants.preview = true;
    let tools = build_self_config_tools(
        node.clone(),
        owner.clone(),
        Some(identity),
        &grants,
        std::sync::Arc::new(crate::plugin::executor::PluginExecutor::default()),
    );
    (node, owner, tools)
}

#[tokio::test]
async fn behavior_ids_resolve_from_their_principal_local_slug() {
    let (node, owner, tools) = setup("slug", &["persona", "tools", "profile"]).await;
    let worker = format!("{owner}:worker-a");
    crate::test_support::install_test_behavior(&node, &owner, &worker).await;

    let read = ok(
        &tools,
        json!({"argv":["tools","get"],"options":{"behavior":"worker-a"}}),
    )
    .await;
    assert_eq!(read["document"]["tools_id"], format!("{worker}:tools"));
    let read = ok(&tools, json!({"argv":["behavior","get","worker-a"]})).await;
    assert!(read.to_string().contains(&worker), "{read}");

    // The receipt names the behavior and document the short ID selected.
    let receipt = ok(
        &tools,
        json!({"argv":["profile","preview"],"options":{"behavior":"worker-a"},"set":{"display_name":"Worker"}}),
    )
    .await;
    assert_eq!(receipt["behavior_id"], worker);
    assert_eq!(receipt["target_id"], format!("{worker}:inference"));

    // With several behaviors, a profile edit must name its target.
    let error = refused(
        &tools,
        json!({"argv":["profile","preview"],"set":{"display_name":"Mine"}}),
    )
    .await;
    assert!(
        error.contains("needs options.behavior") && error.contains(&worker),
        "{error}"
    );

    // A local SubagentTarget stores the resolved ID.
    ok(
        &tools,
        json!({"argv":["subagent-target","create"],"target_id":"worker","set":{"name":"worker","target_agent_did":owner,"behavior_id":"worker-a"}}),
    )
    .await;
    let target = ok(
        &tools,
        json!({"argv":["subagent-target","get"],"target_id":"worker"}),
    )
    .await;
    assert_eq!(target["document"]["behavior_id"], worker);

    // An exact ID wins over a slug that happens to match it.
    crate::test_support::install_test_behavior(&node, &owner, "worker-b").await;
    crate::test_support::install_test_behavior(&node, &owner, &format!("{owner}:worker-b")).await;
    let read = ok(
        &tools,
        json!({"argv":["tools","get"],"options":{"behavior":"worker-b"}}),
    )
    .await;
    assert_eq!(read["document"]["tools_id"], "worker-b:tools");

    // An unknown ID names the next call, with no connected-preview detour.
    let error = refused(
        &tools,
        json!({"argv":["tools","get"],"options":{"behavior":"nope"}}),
    )
    .await;
    assert!(
        error.contains("[\\\"behavior\\\",\\\"list\\\"]") && !error.contains("recovery"),
        "{error}"
    );

    // Create derives the ID; an explicit one would be silently ignored.
    let error = refused(
        &tools,
        json!({"argv":["behavior","create"],"options":{"id":"x","display-name":"X","system-prompt":"p","preset":"readonly","profile":"beh-test:inference"}}),
    )
    .await;
    assert!(error.contains("takes no id"), "{error}");
}

#[tokio::test]
async fn profile_receipts_name_the_default_target_and_preview_edit_means_preview() {
    let (_node, _owner, tools) = setup("profile-target", &["persona", "profile"]).await;
    let receipt = ok(
        &tools,
        json!({"argv":["profile","preview"],"set":{"display_name":"Mine"}}),
    )
    .await;
    assert_eq!(receipt["behavior_id"], "beh-test");
    assert_eq!(receipt["target_id"], "beh-test:inference");
    let aliased = ok(
        &tools,
        json!({"argv":["profile","preview","edit"],"set":{"display_name":"Mine"}}),
    )
    .await;
    assert_eq!(aliased["committed"], false);
    assert_eq!(aliased["target_id"], "beh-test:inference");
}

/// #2133: verbs a model guesses resolve to the command they mean, or the
/// refusal shows the exact working form.
#[tokio::test]
async fn guessed_verbs_resolve_or_name_the_working_form() {
    let (_node, _owner, tools) = setup("verbs", &["persona", "tools", "automation"]).await;
    let task = |verb: &[&str]| {
        let mut argv = vec!["automation"];
        argv.extend_from_slice(verb);
        argv.push("task");
        json!({"argv":argv,"target_id":"review","set":{"prompt_template":"Review."}})
    };
    for verb in [&["preview", "edit"][..], &["preview", "create"][..]] {
        let receipt = ok(&tools, task(verb)).await;
        assert_eq!(receipt["committed"], false, "{verb:?}: {receipt}");
    }
    let created = ok(&tools, task(&["create"])).await;
    assert_eq!(created["committed"], true, "{created}");
    assert_eq!(created["target_id"], "review");

    let tools_preview = ok(
        &tools,
        json!({"argv":["tools","preview","edit"],"set":{"tags":["x"]}}),
    )
    .await;
    assert_eq!(tools_preview["committed"], false);
    for argv in [
        json!(["tools", "create"]),
        json!(["tools", "preview", "create"]),
    ] {
        let error = refused(&tools, json!({"argv":argv,"set":{"tags":["x"]}})).await;
        assert!(
            error.contains(r#"tools are created with their behavior (behavior create); change a behavior's tools with {\"argv\":[\"tools\",\"edit\"],\"options\":{\"behavior\":\"BEHAVIOR_ID\"}"#),
            "{error}"
        );
    }
    let schema = refused(
        &tools,
        json!({"argv":["schema","apply","install"],"options":{"sdl":"type A { a: String }"}}),
    )
    .await;
    assert!(
        schema.contains(r#"schema has no apply verb; use [\"schema\",\"install\"] with options.sdl and options.digest"#),
        "{schema}"
    );
    let automation = refused(&tools, json!({"argv":["automation","apply","task"]})).await;
    assert!(
        automation.contains(r#"automation has no apply verb; use the previewed call with its target_id, options and set, removing \"preview\" from argv and putting \"edit\" in its place when no create or edit follows it"#),
        "{automation}"
    );
}

/// #2133: a decode error names the field path and its advertised shape, not
/// the Rust type serde expected.
#[tokio::test]
async fn type_errors_name_the_field_path_and_shape() {
    let (_node, _owner, tools) = setup("shapes", &["tools"]).await;
    let error = refused(
        &tools,
        json!({"argv":["tools","preview"],"set":{"integrations":{"lsp":"gents-mailbox"}}}),
    )
    .await;
    assert!(
        error.contains(r#"Tools field integrations.lsp: invalid type: string \"gents-mailbox\"; expected {\"config\":\"JSON encoded as a string|null\""#),
        "{error}"
    );
    assert!(!error.contains("LspTools"), "{error}");
    let nested = refused(
        &tools,
        json!({"argv":["tools","preview"],"set":{"host":{"cli":[{"name":7}]}}}),
    )
    .await;
    assert!(
        nested.contains(r#"Tools field host.cli[0].name: invalid type: integer `7`; expected string; host-registered CLI tool name"#),
        "{nested}"
    );
}

/// #2133: a target ID sent in argv and target_id is one ID when they agree.
#[test]
fn target_id_may_repeat_the_argv_id_but_not_conflict() {
    let params = |target_id: &str| {
        serde_json::from_value::<command::ConfigCommandParams>(json!({
            "argv": ["datastore", "edit", "handoff-tools"],
            "target_id": target_id,
        }))
        .unwrap()
    };
    assert_eq!(
        json!(params("handoff-tools").into_argv_for_test().unwrap()),
        json!(["datastore", "edit", "handoff-tools"])
    );
    let conflict = params("other")
        .into_argv_for_test()
        .unwrap_err()
        .to_string();
    assert_eq!(
        conflict,
        r#"target ID "handoff-tools" in argv conflicts with target_id "other"; send one of them"#
    );
}

#[tokio::test]
async fn own_tools_refuse_a_group_set_that_silently_drops_existing_settings() {
    let node = build_persona_node().await;
    let identity = persona_identity("tools-drop");
    let owner = identity.did().to_string();
    for behavior in ["setup", "worker"] {
        crate::test_support::install_test_behavior(&node, &owner, behavior).await;
    }
    let mut grants = config(&["persona", "tools"]);
    grants.behavior_id = "setup".into();
    grants.preview = true;
    grants.no_lockout = true;
    // Surfaces exist before the invoker's Tools gain the lockout-guarded grant.
    let mut unguarded = grants.clone();
    unguarded.no_lockout = false;
    let setup_tools = build_self_config_tools(
        node.clone(),
        owner.clone(),
        Some(identity.clone()),
        &unguarded,
        std::sync::Arc::new(crate::plugin::executor::PluginExecutor::default()),
    );
    let tools = build_self_config_tools(
        node.clone(),
        owner.clone(),
        Some(identity),
        &grants,
        std::sync::Arc::new(crate::plugin::executor::PluginExecutor::default()),
    );
    for surface in ["engineer-mailbox", "worker-surface"] {
        ok(
            &setup_tools,
            json!({"argv":["datastore","create"],"target_id":surface,"set":{"display_name":surface}}),
        )
        .await;
    }
    let setup_core = SelfConfigCore::new(node.clone(), owner.clone(), "setup".into()).unwrap();
    setup_core
        .apply(tools_request(
            &setup_core,
            vec![
                (
                    "self_config".into(),
                    Some(json!({"enable_self_config":true,"self_config_no_lockout":true})),
                ),
                ("subagents".into(), Some(json!({"enabled":true}))),
                (
                    "datastore".into(),
                    Some(json!({"enable_defra_query":true,"datastore_tool_surface_ids":["engineer-mailbox"]})),
                ),
            ],
            false,
        ))
        .await
        .unwrap();

    let partial = json!({"datastore_tool_surface_ids":["worker-surface"]});
    let error = refused(
        &tools,
        json!({"argv":["tools","preview"],"set":{"datastore":partial}}),
    )
    .await;
    assert!(
        error.contains("datastore.enable_defra_query")
            && error.contains("datastore.datastore_tool_surface_ids item \\\"engineer-mailbox\\\"")
            && error.contains("allow-drop"),
        "{error}"
    );
    // The lockout guard still reports a lockout as one.
    let error = refused(
        &tools,
        json!({"argv":["tools","preview"],"set":{"self_config":{"enable_self_config":false}}}),
    )
    .await;
    assert!(error.contains("no-lockout guard"), "{error}");
    // Carrying the group forward, or naming it, is accepted.
    ok(
        &tools,
        json!({"argv":["tools","preview"],"set":{"datastore":{"enable_defra_query":true,"datastore_tool_surface_ids":["engineer-mailbox","worker-surface"]}}}),
    )
    .await;
    ok(
        &tools,
        json!({"argv":["tools","preview"],"options":{"allow-drop":"datastore"},"set":{"datastore":partial}}),
    )
    .await;
    // Another behavior's Tools keep plain replacement semantics.
    ok(
        &tools,
        json!({"argv":["tools","preview"],"options":{"behavior":"worker"},"set":{"datastore":partial}}),
    )
    .await;
}

#[tokio::test]
async fn automation_validation_refuses_what_every_fire_would_reject() {
    let (node, _owner, tools) = setup("automation-gaps", &["automation"]).await;
    node.add_schema("type GapInput { message: String reply_session_id: String }")
        .await
        .unwrap();
    let edit = |kind: &str, id: &str, set: Value| json!({"argv":["automation","edit",kind],"target_id":id,"set":set});

    // An event source on a collection that does not exist never fires.
    let error = refused(
        &tools,
        edit(
            "event-source",
            "missing",
            json!({"source_collection":"NoSuchCollection"}),
        ),
    )
    .await;
    assert!(error.contains("not an installed collection"), "{error}");
    // A stringified object where a native one belongs names the field.
    let error = refused(
        &tools,
        edit(
            "event-source",
            "grouped",
            json!({"source_collection":"GapInput","correlation_field":"message","group":"{\"expected_count\":2}"}),
        ),
    )
    .await;
    assert!(
        error.contains("field \\\"group\\\" holds a JSON string; send a native JSON object"),
        "{error}"
    );
    // A native JSON object is not a filter string; the error names the field.
    let error = refused(
        &tools,
        edit(
            "event-source",
            "input",
            json!({"source_collection":"GapInput","filter":{"message":{"_eq":"x"}}}),
        ),
    )
    .await;
    assert!(
        error.contains("filter must be a string holding a GraphQL object literal"),
        "{error}"
    );
    // A JSON-quoted filter key parses as a string but fails every query.
    let error = refused(
        &tools,
        edit(
            "event-source",
            "input",
            json!({"source_collection":"GapInput","filter":"{\"message\": {\"_eq\": \"x\"}}"}),
        ),
    )
    .await;
    assert!(error.contains("unquoted GraphQL names"), "{error}");
    ok(
        &tools,
        edit(
            "event-source",
            "input",
            json!({"source_collection":"GapInput","filter":"{message: {_ne: \"\"}}"}),
        ),
    )
    .await;

    // #1970: a doc field the source collection lacks fails every fire.
    ok(
        &tools,
        edit(
            "task",
            "work",
            json!({"prompt_template":"Do {{ doc.message }}"}),
        ),
    )
    .await;
    ok(
        &tools,
        edit(
            "trigger",
            "on-input",
            json!({"task_id":"work","source":{"kind":"event","event_source_id":"input"},"session_id_template":"{{ doc.reply_session_id }}"}),
        ),
    )
    .await;
    let error = refused(
        &tools,
        edit(
            "task",
            "work",
            json!({"prompt_template":"Do {{ doc.not_a_field }}"}),
        ),
    )
    .await;
    assert!(
        !error.contains("COUNT")
            && error.contains("doc.not_a_field")
            && error.contains("message, reply_session_id"),
        "{error}"
    );
    // A defaulted field may be absent from this collection.
    ok(
        &tools,
        edit(
            "task",
            "work",
            json!({"prompt_template":"Do {{ doc.message }} {{ doc.priority | default('normal') }}"}),
        ),
    )
    .await;
    let error = refused(
        &tools,
        edit(
            "trigger",
            "on-input",
            json!({"session_id_template":"{{ doc.session }}"}),
        ),
    )
    .await;
    assert!(error.contains("doc.session"), "{error}");

    // #2080: schedule fires cannot target a session or queue.
    ok(
        &tools,
        edit(
            "schedule",
            "hourly",
            json!({"cadence":{"kind":"interval","interval_secs":3600}}),
        ),
    )
    .await;
    ok(
        &tools,
        edit("task", "tick", json!({"prompt_template":"Tick"})),
    )
    .await;
    for (field, value) in [
        ("session_id_template", json!("abc-session")),
        ("concurrency", json!("queued_serial")),
    ] {
        let mut set = json!({"task_id":"tick","source":{"kind":"schedule","schedule_id":"hourly"}});
        set[field] = value;
        let error = refused(&tools, edit("trigger", "hourly-tick", set)).await;
        assert!(error.contains("schedule source"), "{field}: {error}");
    }
    ok(
        &tools,
        edit(
            "trigger",
            "hourly-tick",
            json!({"task_id":"tick","source":{"kind":"schedule","schedule_id":"hourly"}}),
        ),
    )
    .await;
}

#[tokio::test]
async fn execution_deadlines_are_bounded_where_a_claim_can_represent_them() {
    let (_node, _owner, tools) = setup("deadline", &["profile"]).await;
    let create = |deadline: i64| json!({"argv":["execution","preview","create"],"target_id":"long","set":{"deadline_duration_secs":deadline}});
    let error = refused(&tools, create(i64::MAX)).await;
    assert!(error.contains("at most 3153600000"), "{error}");
    ok(
        &tools,
        create(crate::document_config::MAX_DEADLINE_DURATION_SECS),
    )
    .await;
}

#[tokio::test]
async fn help_is_layered_and_its_recipes_run_as_written() {
    let node = build_persona_node().await;
    let identity = persona_identity("recipes");
    let owner = identity.did().to_string();
    crate::test_support::install_test_behavior(&node, &owner, "setup").await;
    crate::test_support::install_test_behavior(&node, &owner, &format!("{owner}:worker")).await;
    let mut grants = config(&["persona", "tools", "profile", "automation"]);
    grants.behavior_id = "setup".into();
    grants.preview = true;
    let tools = build_self_config_tools(
        node.clone(),
        owner.clone(),
        Some(identity.clone()),
        &grants,
        std::sync::Arc::new(crate::plugin::executor::PluginExecutor::default()),
    );

    // Pages are plain text of skill size; nothing from the index repeats.
    let index = call_config_tool(&tools, vec!["help".into()]).await.unwrap();
    assert!(index.len() < 2_000, "help index is {} chars", index.len());
    for resource in [
        "get",
        "behavior",
        "tools",
        "datastore",
        "subagent-target",
        "profile",
        "execution",
        "automation",
        "schema",
        "cleanup",
        "skill",
        "discovery",
        "plan",
    ] {
        let page = call_config_tool(&tools, vec!["help".into(), resource.into()])
            .await
            .unwrap();
        assert!(page.starts_with(&format!("{resource}: ")), "{page}");
        assert!(page.contains("\nNext: "), "{resource}: {page}");
        assert!(!page.contains("config_execution") && !page.contains("Preview: tools"));
        assert!(
            page.len() <= 2_600,
            "{resource} help is {} chars",
            page.len()
        );
    }
    for alias in ["task", "trigger", "event-source", "agent", "graph"] {
        call_config_tool(&tools, vec!["help".into(), alias.into()])
            .await
            .unwrap_or_else(|error| panic!("{alias}: {error}"));
    }

    // Behavior create is applied by the persona reconciler.
    let store = crate::agent::p2p_reconcile::GraphqlPersonaRequestStore::with_local_identity(
        node.clone(),
        None,
        identity,
    );
    let ticker_node = node.clone();
    let ticker = tokio::spawn(async move {
        loop {
            let _ = crate::agent::p2p_reconcile::reconcile_persona_tick(&store, &ticker_node).await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    let substitute = |call: &Value| -> Value {
        let text = call
            .to_string()
            .replace("<BACKEND_ID>", "setup:backend")
            .replace("<MODEL>", "test-model")
            .replace("<PROMPT>", "Lead the work.")
            .replace("<DID>", &owner);
        serde_json::from_str(&text).unwrap()
    };
    for resource in ["behavior", "execution", "datastore", "automation"] {
        for (title, steps) in command::help::recipes(resource) {
            for (call, _) in steps {
                let call = substitute(&call);
                let result = ok(&tools, call.clone()).await;
                // The schema step previews; install it as its note says.
                if call["argv"] == json!(["schema", "preview", "install"]) {
                    let mut install = call.clone();
                    install["argv"] = json!(["schema", "install"]);
                    install["options"]["digest"] = result["plan"]["artifact_digest"].clone();
                    ok(&tools, install).await;
                }
                assert!(!result.is_null(), "{resource} recipe {title}");
            }
        }
    }
    ticker.abort();
    let lead = ok(&tools, json!({"argv":["behavior","get","lead"]})).await;
    assert!(
        lead.to_string().contains(&format!("{owner}:lead")),
        "{lead}"
    );
}

/// Model-facing results read top to bottom: the answer, then how to proceed,
/// then metadata. Pins top-level key order of help, receipts and errors.
#[tokio::test]
async fn results_put_the_answer_first_and_execution_metadata_last() {
    let (_node, _owner, tools) = setup("key-order", &["persona", "profile"]).await;
    let tool = tools
        .iter()
        .find(|tool| tool.name() == CONFIG_TOOL_NAME)
        .unwrap();
    let order = |text: &str, keys: &[&str]| {
        let positions = keys
            .iter()
            .map(|key| {
                text.find(&format!("\"{key}\""))
                    .unwrap_or_else(|| panic!("{key} missing: {text}"))
            })
            .collect::<Vec<_>>();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{keys:?} out of order: {text}"
        );
    };

    let help = tool
        .call(json!({"argv":["help","profile"]}).to_string())
        .await
        .unwrap();
    assert!(help.starts_with("profile: "), "{help}");

    let receipt = tool
        .call(json!({"argv":["profile","preview"],"set":{"display_name":"Mine"}}).to_string())
        .await
        .unwrap();
    order(
        &receipt,
        &[
            "committed",
            "collection",
            "target_id",
            "behavior_id",
            "changed",
            "effect",
            "config_execution",
        ],
    );
    assert!(receipt.trim_end().ends_with('}') && serde_json::from_str::<Value>(&receipt).is_ok());

    let error = match tool
        .call(json!({"argv":["profile","preview"],"options":{"behavior":"nope"},"set":{"display_name":"x"}}).to_string())
        .await
    {
        Err(crate::llm::tool::ToolError::ToolCallError(error)) => error.to_string(),
        other => panic!("expected an error: {other:?}"),
    };
    order(&error, &["error", "recovery", "config_execution"]);
}

#[test]
fn a_list_of_strings_in_options_is_the_repeated_flag() {
    let params: command::ConfigCommandParams = serde_json::from_value(json!({
        "argv": ["cleanup", "preview"],
        "options": {"target": ["task=a", "trigger=b"]}
    }))
    .unwrap();
    assert_eq!(
        json!(params.into_argv_for_test().unwrap()),
        json!([
            "cleanup",
            "preview",
            "--target",
            "task=a",
            "--target",
            "trigger=b"
        ])
    );
}

/// Ladder findings: reads a model guesses name the working read form.
#[tokio::test]
async fn guessed_lists_name_the_working_read() {
    let (_node, _owner, tools) = setup("lists", &["persona", "tools", "automation"]).await;
    for (args, form) in [
        (
            json!({"argv":["tools","list"]}),
            r#"tools has no list: each behavior has one; read it with {\"argv\":[\"tools\",\"get\"],\"options\":{\"behavior\":\"BEHAVIOR_ID\"}}"#,
        ),
        (
            json!({"argv":["automation","list","task"]}),
            r#"automation has no list; a behavior's event sources, schedules, triggers and tasks are in {\"argv\":[\"get\"],\"options\":{\"behavior\":\"BEHAVIOR_ID\"}} under automation"#,
        ),
        (
            json!({"argv":["datastore","get"]}),
            r#"surface IDs are listed in a behavior's Tools: read {\"argv\":[\"tools\",\"get\"],\"options\":{\"behavior\":\"BEHAVIOR_ID\"}}, then [\"datastore\",\"get\",SURFACE_ID]"#,
        ),
        (
            json!({"argv":["datastore","list"]}),
            r#"surface IDs are listed in a behavior's Tools"#,
        ),
    ] {
        let error = refused(&tools, args.clone()).await;
        assert!(error.contains(form), "{args}: {error}");
    }
}

/// Ladder finding: target_id on a command with a positional ID is that ID.
#[tokio::test]
async fn target_id_fills_a_positional_id() {
    let (_node, _owner, tools) = setup("positional", &["persona", "tools"]).await;
    for args in [
        json!({"argv":["behavior","get"],"target_id":"beh-test"}),
        json!({"argv":["behavior","get","beh-test"],"target_id":"beh-test"}),
    ] {
        let read = ok(&tools, args.clone()).await;
        assert_eq!(read["behavior_id"], "beh-test", "{args}");
    }
    let conflict = refused(
        &tools,
        json!({"argv":["behavior","get","beh-test"],"target_id":"other"}),
    )
    .await;
    assert!(
        conflict.contains(r#"target ID \"beh-test\" in argv conflicts with target_id \"other\""#),
        "{conflict}"
    );
    let unsupported = refused(&tools, json!({"argv":["tools","get"],"target_id":"x"})).await;
    assert!(
        unsupported.contains("target_id is not accepted by tools")
            && unsupported.contains("options.behavior"),
        "{unsupported}"
    );
    let recovered = ok(
        &tools,
        json!({"argv":["tools","get"],"options":{"behavior":"beh-test"}}),
    )
    .await;
    assert_eq!(recovered["document"]["tools_id"], "beh-test:tools");
}

/// Ladder finding: an unknown plan collection is named and classified.
#[tokio::test]
async fn plan_names_an_unknown_collection_and_what_it_is() {
    let (_node, owner, tools) = setup("plan-collection", &["persona", "tools"]).await;
    let plan = |collection: &str| json!({"argv":["plan","preview"],"options":{"documents":[{"collection":collection,"document":{"agent_did":owner,"tools_id":"t"}}]}});
    for (collection, expected) in [
        (
            "agent_behavior",
            r#"unknown collection \"agent_behavior\"; did you mean \"AgentBehavior\"?"#,
        ),
        (
            "Behavior",
            r#"unknown collection \"Behavior\"; did you mean \"AgentBehavior\"?"#,
        ),
        (
            "AgentRequest",
            r#"\"AgentRequest\" is an application collection, not configuration; plan preview stages only AgentBehavior"#,
        ),
        (
            "Handoff",
            r#"\"Handoff\" is neither a configuration collection nor an installed schema"#,
        ),
    ] {
        let error = refused(&tools, plan(collection)).await;
        assert!(error.contains(expected), "{collection}: {error}");
    }
}

/// Mailbox suite: help states the identity choice where a policy is written,
/// and the monitor example uses condition identity.
#[tokio::test]
async fn mailbox_help_states_the_identity_choice() {
    let (_node, _owner, tools) = setup("mailbox-help", &["tools"]).await;
    let choice = "condition identity: one open item per stable finding, updated across requests; event identity: a new item for every request";
    for argv in [
        json!(["help", "datastore"]),
        json!(["datastore", "create", "--help"]),
    ] {
        let help = ok(&tools, json!({"argv":argv})).await;
        let help = help.as_str().unwrap();
        assert!(help.contains(choice), "{argv}: {help}");
    }
    let page = ok(&tools, json!({"argv":["help","datastore"]})).await;
    assert!(
        page.as_str().unwrap().contains(r#"A monitor uses condition identity: {"argv":["datastore","create"],"target_id":"monitor-mailbox","options":{"mailbox":{"identity":{"mode":"condition","key":"host-health"}"#),
        "{page}"
    );
}

/// L3 datastore finding: the page says what fill means, with a caller key
/// named correlation and a runtime-filled request_correlation.
#[tokio::test]
async fn datastore_help_says_what_fill_means() {
    let (_node, _owner, tools) = setup("fill-help", &["tools"]).await;
    let page = ok(&tools, json!({"argv":["help","datastore"]})).await;
    let page = page.as_str().unwrap();
    for line in [
        "fill: correlation uses the request/trigger correlation ID",
        "Omit fill for caller-supplied values",
        "omitted or empty means none",
        "current request keeps its existing tools",
        r#"Caller value: {"name":"correlation"}. Runtime ID: {"name":"request_correlation","fill":"correlation"}"#,
    ] {
        assert!(page.contains(line), "{line}\n{page}");
    }
    let shapes = ok(&tools, json!({"argv":["datastore","create","--help"]})).await;
    assert!(
        shapes
            .as_str()
            .unwrap()
            .contains(r#"fill fields are runtime-filled and never model arguments (what fill means: [\"help\",\"datastore\"])"#),
        "{shapes}"
    );
}
