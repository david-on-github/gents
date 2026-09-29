//! The factory-setup eval (#1786): a definition pack and one subject pack per
//! kickoff variant, run with `gents eval run factory-setup`. These tests keep
//! the packs loadable, the definition valid against the shipped check
//! registry, and each subject's Engineer the Engineer the desktop first run
//! creates, so a variant differs only in its `factory/` kickoff files.

use std::path::{Path, PathBuf};

use gents::document_config::{EvalDefinition, PackConfig, SurfaceToolDecl};
use gents::eval::checks::CheckRegistry;
use gents::eval::runner::ScriptedExecutor;
use gents::eval::OutcomeKind;
use gents::pack::{declared_paths, load_pack_config, PackInstallOptions, PackManifest};

const OWNER: &str = "did:key:zFactorySetupEvalOwner";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/configurator_evals/factory_setup")
}

/// Every directory under the fixture root holding a subject pack.
fn subjects() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("manifest.json").exists() && !path.ends_with("definition"))
        .collect();
    dirs.sort();
    assert!(
        dirs.len() >= 2,
        "expected one subject pack per variant: {dirs:?}"
    );
    dirs
}

fn load(dir: &Path) -> (PackManifest, PackConfig) {
    let manifest: PackManifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    for path in declared_paths(&manifest) {
        assert!(
            dir.join(&path).is_file(),
            "{} declares missing {path}",
            dir.display()
        );
    }
    let config = load_pack_config(
        &manifest,
        &PackInstallOptions {
            agent_did: OWNER.into(),
        },
        &|path| Ok(std::fs::read(dir.join(path))?),
        &|_| None,
    )
    .unwrap_or_else(|error| panic!("{}: {error:#}", dir.display()));
    (manifest, config)
}

#[test]
fn every_subject_is_the_desktop_engineer_on_an_eval_node() {
    for dir in subjects() {
        let (manifest, config) = load(&dir);
        let name = dir.display();
        let [behavior] = config.agent_behaviors.as_slice() else {
            panic!("{name}: one behavior, the Engineer");
        };
        assert_eq!(behavior.behavior_id, "engineer", "{name}");
        // A trial runtime becomes ready only for a principal that names its
        // default behavior.
        assert_eq!(
            config.agent_principal.default_behavior_id.as_deref(),
            Some("engineer"),
            "{name}"
        );
        assert!(
            behavior
                .tags
                .iter()
                .any(|tag| tag == gents::agent::persona_ops::SETUP_STEWARD_BEHAVIOR_TAG),
            "{name}: the Engineer carries the protected Setup tag"
        );
        assert_eq!(
            manifest.metadata.inference_slots.len(),
            1,
            "{name}: one slot binds the Engineer and, through its backend, the crew"
        );
        let context = &config.contexts[0];
        // A variant may trim the shipped prompt only when its directory says
        // so; every other subject runs the prompt the desktop ships.
        if !dir.to_string_lossy().contains("trimmed") {
            assert_eq!(
                context.system_prompt.as_deref(),
                Some(gents_protocol::SETUP_STEWARD_PROMPT),
                "{name}: engineer/system_prompt.md must equal the shipped Setup prompt"
            );
        }
        let tools = &config.tools[0];
        assert_eq!(
            tools.self_config,
            Some(gents::agent::persona_ops::setup_steward_self_config()),
            "{name}: the Setup self-config grant"
        );
        assert_eq!(
            tools.subagents.as_ref().and_then(|agents| agents.enabled),
            Some(true),
            "{name}"
        );
        let built_ins = tools.built_ins.as_ref().unwrap();
        assert_eq!(built_ins.enable_graph_tools, Some(true), "{name}");
        assert_eq!(built_ins.enable_session_history_tool, Some(true), "{name}");
        let datastore = tools.datastore.as_ref().unwrap();
        assert_eq!(datastore.enable_defra_query, Some(true), "{name}");
        assert_eq!(
            datastore.datastore_tool_surface_ids.as_deref(),
            Some(&["engineer-mailbox".to_string()][..]),
            "{name}"
        );
        // Embedded trials refuse host bash; the Engineer configures through
        // the native config tool and reads its kickoff with file tools.
        let host = tools.host.as_ref().unwrap();
        assert_eq!(
            host.bash.as_ref().map(|bash| bash.mode),
            Some(gents::tool_surface::BashMode::Off),
            "{name}"
        );
        let [surface] = config.datastore_tool_surfaces.as_slice() else {
            panic!("{name}: one surface, engineer-mailbox");
        };
        let expected = vec![SurfaceToolDecl::Create(
            gents::mailbox::canonical_mailbox_write_decl(),
        )];
        assert_eq!(
            surface.entries.as_ref(),
            Some(&expected),
            "{name}: the canonical mailbox declaration, serialized as {}",
            serde_json::to_string(&expected).unwrap()
        );
        assert!(
            dir.join("factory/kickoff.md").is_file(),
            "{name}: every variant's kickoff is factory/kickoff.md"
        );
    }
}

fn definition() -> EvalDefinition {
    let (_, config) = load(&root().join("definition"));
    let [definition] = config.eval_definitions.as_slice() else {
        panic!("the definition pack carries one eval definition");
    };
    definition.clone()
}

#[test]
fn the_definition_validates_and_every_check_accepts_its_params() {
    let definition = definition();
    definition.validate().unwrap();
    assert_eq!(definition.definition_id, "factory-setup");
    let registry = CheckRegistry::builtin();
    let [case] = definition.cases.as_slice() else {
        panic!("one case");
    };
    assert_eq!(case.fixtures.as_ref().unwrap().assets, ["factory"]);
    let [stage] = case.stages.as_slice() else {
        panic!("one stage");
    };
    assert!(
        stage.settle,
        "the crew's handoffs finish after the Engineer's first turn"
    );
    // The harness gate is reported, never scored: a Gents defect must not
    // turn the model's grade into missing evidence.
    let gate = stage
        .checks
        .iter()
        .find(|check| check.check == "handoff_delivery")
        .expect("the harness gate");
    assert_eq!(gate.tier, gents::document_config::EvalTier::Development);
    // Evidence with every capture empty: each check must reach a verdict about
    // the subject (or say no fire happened), never reject its own params.
    let mut evidence =
        ScriptedExecutor::passed_evidence(OWNER, &stage.stage_id, "unused", Vec::new())
            .stages
            .remove(0);
    evidence.captures.clear();
    for capture in &stage.capture {
        evidence.captures.insert(
            capture.name().to_string(),
            gents::eval::runner::CaptureResult::Documents { rows: Vec::new() },
        );
    }
    for check in &stage.checks {
        let implementation = registry
            .get(&check.check)
            .unwrap_or_else(|| panic!("{} is not a shipped check", check.check));
        let verdict = implementation.evaluate(&check.params, &evidence);
        assert_ne!(
            verdict.kind,
            OutcomeKind::Grader,
            "{} rejected its params: {}",
            check.check,
            verdict.raw
        );
    }
}

#[test]
fn the_crew_spec_names_every_id_the_baseline_kickoff_prescribes() {
    let spec: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root().join("engineer_v2/factory/crew_spec.json")).unwrap(),
    )
    .unwrap();
    let text = spec.to_string();
    let kickoff = std::fs::read_to_string(root().join("engineer_v1/factory/kickoff.md")).unwrap();
    let table = kickoff
        .split("### IDs to use")
        .nth(1)
        .expect("the baseline kickoff lists the IDs it prescribes");
    let ids: Vec<&str> = table
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|id| !id.contains('<') && !id.contains(' '))
        .collect();
    assert!(ids.len() > 40, "{ids:?}");
    for id in ids {
        assert!(
            text.contains(&format!("\"{id}\"")),
            "crew_spec.json lacks the baseline ID {id}"
        );
    }
}
