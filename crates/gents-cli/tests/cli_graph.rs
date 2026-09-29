//! Checkout-independent binary acceptance for bundled graph discovery and
//! revision-backed installation. Runtime/model execution has its own live
//! fixture; these cases keep the package boundary honest in the ordinary CLI.

mod support;

use anyhow::{Context, Result};
use serde_json::Value;

use support::{
    agent_did_from_init, allocate_port, run_cli_failure_stderr, run_cli_json, run_cli_text,
    run_init_json, spawn_server_with_ready_json,
};

fn required_str<'a>(value: &'a Value, path: &[&str]) -> Result<&'a str> {
    let mut current = value;
    for segment in path {
        current = current
            .get(*segment)
            .with_context(|| format!("missing JSON path {} in {value}", path.join(".")))?;
    }
    current
        .as_str()
        .with_context(|| format!("JSON path {} is not a string", path.join(".")))
}

#[test]
fn all_pack_kinds_are_available_without_a_checkout() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let catalog = run_cli_json(temp.path(), &["pack", "list"])?;
    anyhow::ensure!(catalog["packs"].as_array().context("packs")?.len() >= 11);
    for name in ["code_review", "pipeline", "mailbox", "graph_pipeline"] {
        let shown = run_cli_json(temp.path(), &["pack", "show", name])?;
        anyhow::ensure!(shown["manifest"]["name"] == name);
    }
    let root = temp.path().join("assets");
    let root_arg = root.to_str().context("path")?;
    let denial = run_cli_failure_stderr(
        temp.path(),
        &[
            "pack", "install", "mailbox", "--home", root_arg, "--output", "text",
        ],
    )?;
    anyhow::ensure!(denial.contains("unsupported --output text"), "{denial}");
    anyhow::ensure!(!root.exists(), "invalid output format wrote pack assets");
    for flag in ["--force-rebind-concrete-did", "--agent-did"] {
        let mut invalid = vec!["pack", "install", "mailbox", "--home", root_arg, flag];
        if flag == "--agent-did" {
            invalid.push("did:key:unused");
        }
        let denial = run_cli_failure_stderr(temp.path(), &invalid)?;
        anyhow::ensure!(denial.contains("binding flags do not apply"), "{denial}");
        anyhow::ensure!(!root.exists(), "invalid options wrote pack assets");
    }
    let args = ["pack", "install", "mailbox", "--home", root_arg];
    let first = run_cli_json(temp.path(), &args)?;
    anyhow::ensure!(run_cli_json(temp.path(), &args)? == first);
    let installed = std::path::Path::new(required_str(&first, &["installed_assets"])?);
    anyhow::ensure!(installed
        .join("datastore_tool_surfaces/mailbox_writes/object.json")
        .is_file());
    std::fs::write(installed.join("README.md"), "operator edit")?;
    let denial = run_cli_failure_stderr(temp.path(), &args)?;
    anyhow::ensure!(denial.contains("installed asset was modified"));
    Ok(())
}

#[test]
fn document_pack_installs_without_seeding_and_is_idempotent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("node");
    let home_arg = home.to_str().context("path")?;
    run_init_json(
        temp.path(),
        &["--agent-name", "pack-installer", "--home", home_arg],
    )?;
    let args = [
        "pack",
        "install",
        "pipeline",
        "--home",
        home_arg,
        "--force-rebind-concrete-did",
    ];
    let first = run_cli_json(temp.path(), &args)?;
    anyhow::ensure!(first["apply"]["counts"]["AgentBehavior"] == 2, "{first}");
    let before_root = temp.path().join("before");
    run_cli_text(
        temp.path(),
        &[
            "config",
            "export",
            "--home",
            home_arg,
            "--root",
            before_root.to_str().context("export path")?,
        ],
    )?;
    let before = support::read_json_file(&before_root.join("pack_config.json"))?;
    let second = run_cli_json(temp.path(), &args)?;
    anyhow::ensure!(
        second["apply"]["counts"] == first["apply"]["counts"]
            && second["apply"]["created"] == serde_json::json!([])
            && second["apply"]["replaced"] == first["apply"]["created"]
            && second["digest"] == first["digest"],
        "{second}"
    );
    let after_root = temp.path().join("after");
    run_cli_text(
        temp.path(),
        &[
            "config",
            "export",
            "--home",
            home_arg,
            "--root",
            after_root.to_str().context("export path")?,
        ],
    )?;
    anyhow::ensure!(
        before == support::read_json_file(&after_root.join("pack_config.json"))?,
        "reinstall changed canonical configuration"
    );
    Ok(())
}

/// A documents pack that ships a plugin: install stores the plugin and
/// records it, and remove takes back exactly what the install created.
#[test]
fn a_document_pack_with_a_plugin_installs_and_removes_completely() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("node");
    let home_arg = home.to_str().context("path")?;
    run_init_json(
        temp.path(),
        &["--agent-name", "pack-remover", "--home", home_arg],
    )?;

    let gents = |args: &[&str]| -> Result<std::process::Output> {
        Ok(std::process::Command::new(support::cli_bin())
            .env("HOME", temp.path())
            .env("RUST_LOG", "error")
            .current_dir(temp.path())
            .args(args)
            .output()?)
    };
    for args in [
        &["pack", "new", "demo_tools"][..],
        &["pack", "add", "plugin", "echo_tool", "--dir", "demo_tools"],
        &["pack", "build", "demo_tools", "--out", "demo_tools.pack"],
    ] {
        let output = gents(args)?;
        anyhow::ensure!(
            output.status.success(),
            "gents {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let install = run_cli_json(
        temp.path(),
        &[
            "pack",
            "install",
            temp.path()
                .join("demo_tools.pack")
                .to_str()
                .context("path")?,
            "--home",
            home_arg,
        ],
    )?;
    anyhow::ensure!(
        install["apply"]["plugins"][0]["name"] == "echo_tool"
            && !install["apply"]["created"]
                .as_array()
                .context("created")?
                .is_empty(),
        "{install}"
    );
    let echoed = run_cli_json(
        temp.path(),
        &[
            "plugin",
            "run",
            "gents/echo_tool",
            "--home",
            home_arg,
            "--input",
            r#"{"a":1}"#,
        ],
    )?;
    anyhow::ensure!(echoed == serde_json::json!({"a": 1}), "{echoed}");

    let removed = run_cli_json(
        temp.path(),
        &["pack", "remove", "demo_tools", "--home", home_arg],
    )?;
    let sorted = |value: &Value| -> Result<Vec<String>> {
        let mut names: Vec<String> = serde_json::from_value(value.clone())?;
        names.sort();
        Ok(names)
    };
    anyhow::ensure!(
        sorted(&removed["removed"]["removed"])? == sorted(&install["apply"]["created"])?,
        "{removed}"
    );
    let gone = run_cli_failure_stderr(
        temp.path(),
        &[
            "plugin",
            "run",
            "gents/echo_tool",
            "--home",
            home_arg,
            "--input",
            "{}",
        ],
    )?;
    anyhow::ensure!(gone.contains("not installed"), "{gone}");
    let again = run_cli_failure_stderr(
        temp.path(),
        &["pack", "remove", "demo_tools", "--home", home_arg],
    )?;
    anyhow::ensure!(again.contains("is not installed"), "{again}");
    Ok(())
}

#[test]
fn bundled_catalog_is_read_only_outside_a_source_checkout() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating graph catalog tempdir")?;
    let catalog = run_cli_json(tempdir.path(), &["pack", "show", "code_review"])?;
    let package = &catalog["manifest"];
    anyhow::ensure!(
        package.get("name").and_then(Value::as_str) == Some("code_review"),
        "catalog did not return code_review: {catalog}"
    );
    anyhow::ensure!(
        std::fs::read_dir(tempdir.path())?.next().is_none(),
        "read-only catalog created files in a clean working directory"
    );
    Ok(())
}

#[test]
fn web_deep_research_is_in_the_bundled_catalog() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating graph catalog tempdir")?;
    let catalog = run_cli_json(tempdir.path(), &["pack", "show", "web_deep_research"])?;
    let package = &catalog["manifest"];
    anyhow::ensure!(
        package.get("name").and_then(Value::as_str) == Some("web_deep_research"),
        "catalog did not return web_deep_research: {catalog}"
    );
    anyhow::ensure!(
        package.get("config").and_then(Value::as_str) == Some("pack_config.json"),
        "catalog package did not expose its canonical config: {catalog}"
    );
    let dependencies = package
        .get("external_dependencies")
        .and_then(Value::as_array)
        .context("catalog package did not expose external dependencies")?;
    anyhow::ensure!(
        dependencies.len() == 1
            && dependencies[0].get("service_id").and_then(Value::as_str)
                == Some("web-research-mcp")
            && dependencies[0]
                .get("install_command")
                .and_then(Value::as_str)
                == Some("./scripts/stack install-mcp"),
        "catalog package exposed the wrong external dependency: {catalog}"
    );
    Ok(())
}

#[test]
fn clean_binary_install_is_idempotent_activates_and_is_owner_fenced() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating graph install tempdir")?;
    let home = tempdir.path().join("agent-home");
    let home_arg = home.to_str().context("graph home path is not UTF-8")?;
    let initialized = run_init_json(
        tempdir.path(),
        &["--agent-name", "graph-reviewer", "--home", home_arg],
    )?;
    let owner_did = agent_did_from_init(&initialized)?;
    let port = allocate_port()?;
    let (_server, readiness) =
        spawn_server_with_ready_json(&home, port, &["--home", home_arg], &[])?;
    anyhow::ensure!(
        readiness.get("status").and_then(Value::as_str) == Some("serving"),
        "server did not become ready: {readiness}"
    );

    let profile = format!("{owner_did}:default-profile");
    let coordinator = format!("coordinator={profile}");
    let worker = format!("worker={profile}");
    let verifier = format!("verifier={profile}");
    let install_args = [
        "pack",
        "install",
        "code_review",
        "--home",
        home_arg,
        "--output",
        "json",
        "--inference-slot",
        &coordinator,
        "--inference-slot",
        &worker,
        "--inference-slot",
        &verifier,
    ];
    let first = run_cli_json(tempdir.path(), &install_args)?;
    let second = run_cli_json(tempdir.path(), &install_args)?;
    anyhow::ensure!(
        first.get("install") == second.get("install"),
        "repeated install changed its durable receipt\nfirst: {first}\nsecond: {second}"
    );
    let revision = required_str(&first, &["install", "revision_digest"])?;
    anyhow::ensure!(
        required_str(&first, &["activation", "active_digest"])? == revision,
        "install did not activate its exact immutable revision: {first}"
    );

    let wrong_actor = "did:key:z6MkvGraphPackageIntruder";
    let denial = run_cli_failure_stderr(
        tempdir.path(),
        &[
            "pack",
            "install",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            wrong_actor,
        ],
    )?;
    anyhow::ensure!(
        denial.contains("package owner principal is missing"),
        "wrong-owner install did not fail at the identity boundary: {denial}"
    );

    let disabled = run_cli_json(
        tempdir.path(),
        &[
            "graph",
            "disable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(disabled.get("enabled") == Some(&Value::Bool(false)));
    let enabled = run_cli_json(
        tempdir.path(),
        &[
            "graph",
            "enable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(enabled.get("enabled") == Some(&Value::Bool(true)));
    Ok(())
}

/// A documents pack whose graph dependency installs into the same offline
/// home under the one store claim the pack install holds, while a second
/// holder of that home is still refused.
#[test]
fn offline_pack_install_with_a_graph_dependency_holds_one_store_claim() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating pack install tempdir")?;
    let home = tempdir.path().join("agent-home");
    let home_arg = home.to_str().context("pack home path is not UTF-8")?;
    let initialized = run_init_json(
        tempdir.path(),
        &["--agent-name", "port-installer", "--home", home_arg],
    )?;
    let owner_did = agent_did_from_init(&initialized)?;
    let profile = format!("{owner_did}:default-profile");
    let slots: Vec<String> = ["coordinator", "worker", "verifier", "reviewer"]
        .iter()
        .map(|slot| format!("{slot}={profile}"))
        .collect();
    let mut install_args = vec!["pack", "install", "grok_tui_port", "--home", home_arg];
    for slot in &slots {
        install_args.extend(["--inference-slot", slot.as_str()]);
    }

    let installed = run_cli_json(tempdir.path(), &install_args)?;
    anyhow::ensure!(
        installed["dependencies"] == serde_json::json!(["code_review"])
            && installed["owner"] == owner_did.as_str(),
        "pack install did not report its graph dependency: {installed}"
    );
    let enabled = run_cli_json(
        tempdir.path(),
        &[
            "graph",
            "enable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(
        enabled.get("enabled") == Some(&Value::Bool(true)),
        "the graph dependency was not installed: {enabled}"
    );

    let _held = gents::home::lock_store(&home, &gents::home::default_data_dir(&home))?;
    let denial = run_cli_failure_stderr(tempdir.path(), &install_args)?;
    anyhow::ensure!(
        denial.contains("another Gents runtime")
            && denial.contains(&format!("process {}", std::process::id())),
        "a second holder of the home was not refused: {denial}"
    );
    Ok(())
}

/// An assets pack installs and removes with no `gents init` ever run: no
/// node is opened for either operation.
#[test]
fn an_assets_pack_removes_completely_from_an_uninitialized_home() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("assets-home");
    let home_arg = home.to_str().context("path")?;

    let install = run_cli_json(
        temp.path(),
        &["pack", "install", "mailbox", "--home", home_arg],
    )?;
    let installed_assets = std::path::Path::new(required_str(&install, &["installed_assets"])?);
    anyhow::ensure!(installed_assets.is_dir(), "{install}");
    anyhow::ensure!(
        !home.join("data").exists(),
        "an assets-only install never opens a node"
    );

    let removed = run_cli_json(
        temp.path(),
        &["pack", "remove", "mailbox", "--home", home_arg],
    )?;
    anyhow::ensure!(
        removed["removed"]["assets"]
            .as_array()
            .context("assets")?
            .len()
            == 1,
        "{removed}"
    );
    anyhow::ensure!(!installed_assets.exists(), "the cache version was removed");
    anyhow::ensure!(
        !home.join("data").exists(),
        "removal never opened a node either"
    );

    let again = run_cli_failure_stderr(
        temp.path(),
        &["pack", "remove", "mailbox", "--home", home_arg],
    )?;
    anyhow::ensure!(again.contains("is not installed"), "{again}");
    Ok(())
}

/// A graph pack removes completely (its `GraphDefinition`/`GraphRevision`
/// and derived triggers are gone) and reinstalls cleanly afterward.
#[test]
fn a_graph_pack_removes_completely_and_reinstalls() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating graph remove tempdir")?;
    let home = tempdir.path().join("agent-home");
    let home_arg = home.to_str().context("path")?;
    let initialized = run_init_json(
        tempdir.path(),
        &["--agent-name", "graph-remover", "--home", home_arg],
    )?;
    let owner_did = agent_did_from_init(&initialized)?;
    let profile = format!("{owner_did}:default-profile");
    let install_args = [
        "pack",
        "install",
        "code_review",
        "--home",
        home_arg,
        "--inference-slot",
        &format!("coordinator={profile}"),
        "--inference-slot",
        &format!("worker={profile}"),
        "--inference-slot",
        &format!("verifier={profile}"),
    ];
    run_cli_json(tempdir.path(), &install_args)?;

    let removed = run_cli_json(
        tempdir.path(),
        &["pack", "remove", "code_review", "--home", home_arg],
    )?;
    let retained = removed["removed"]["retained"]
        .as_array()
        .context("retained")?;
    anyhow::ensure!(
        retained.iter().any(|entry| entry["item"]
            .as_str()
            .unwrap_or_default()
            .starts_with("schema ")),
        "the package's SDL schema must be reported retained, not silently dropped: {removed}"
    );

    let denial = run_cli_failure_stderr(
        tempdir.path(),
        &[
            "graph",
            "enable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(
        denial.contains("package graph is not installed"),
        "{denial}"
    );

    run_cli_json(tempdir.path(), &install_args)?;
    Ok(())
}

/// A documents pack's graph dependency is tracked: removing the dependency
/// directly is refused while its dependent is installed; removing the
/// dependent releases it; an explicit install of the same coordinate
/// survives removing the dependent that also names it.
#[test]
fn a_graph_dependency_is_released_with_its_dependent() -> Result<()> {
    let tempdir = tempfile::tempdir().context("creating dependency remove tempdir")?;
    let home = tempdir.path().join("agent-home");
    let home_arg = home.to_str().context("path")?;
    let initialized = run_init_json(
        tempdir.path(),
        &["--agent-name", "dep-remover", "--home", home_arg],
    )?;
    let owner_did = agent_did_from_init(&initialized)?;
    let profile = format!("{owner_did}:default-profile");
    let slots: Vec<String> = ["coordinator", "worker", "verifier", "reviewer"]
        .iter()
        .map(|slot| format!("{slot}={profile}"))
        .collect();
    let mut install_args = vec![
        "pack".to_owned(),
        "install".to_owned(),
        "grok_tui_port".to_owned(),
        "--home".to_owned(),
        home_arg.to_owned(),
    ];
    for slot in &slots {
        install_args.push("--inference-slot".to_owned());
        install_args.push(slot.clone());
    }
    let install_args_ref: Vec<&str> = install_args.iter().map(String::as_str).collect();
    run_cli_json(tempdir.path(), &install_args_ref)?;

    let denial = run_cli_failure_stderr(
        tempdir.path(),
        &["pack", "remove", "code_review", "--home", home_arg],
    )?;
    anyhow::ensure!(denial.contains("gents/grok_tui_port"), "{denial}");

    let removed = run_cli_json(
        tempdir.path(),
        &["pack", "remove", "grok_tui_port", "--home", home_arg],
    )?;
    anyhow::ensure!(
        removed["removed"]["dependencies"][0]["pack"] == "gents/code_review",
        "{removed}"
    );

    let denial = run_cli_failure_stderr(
        tempdir.path(),
        &[
            "graph",
            "enable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(
        denial.contains("package graph is not installed"),
        "{denial}"
    );

    // An explicit install of the (now-removed) dependency survives a later
    // removal of the dependent that names it again.
    let explicit_code_review = [
        "pack",
        "install",
        "code_review",
        "--home",
        home_arg,
        "--inference-slot",
        &format!("coordinator={profile}"),
        "--inference-slot",
        &format!("worker={profile}"),
        "--inference-slot",
        &format!("verifier={profile}"),
    ];
    run_cli_json(tempdir.path(), &explicit_code_review)?;
    run_cli_json(tempdir.path(), &install_args_ref)?;
    run_cli_json(
        tempdir.path(),
        &["pack", "remove", "grok_tui_port", "--home", home_arg],
    )?;
    let enabled = run_cli_json(
        tempdir.path(),
        &[
            "graph",
            "enable",
            "code_review",
            "--home",
            home_arg,
            "--agent-did",
            &owner_did,
        ],
    )?;
    anyhow::ensure!(
        enabled.get("enabled") == Some(&Value::Bool(true)),
        "an explicit install of the dependency must survive removing its dependent: {enabled}"
    );
    Ok(())
}

/// `GENTS_CODE_REVIEW_PACK_DIR` must name a checkout of the packs-repo
/// `code_review` pack (gents-ai/packs, `packs/gents/code_review`) built with
/// its `review_evidence` plugin's `.afb` present. This is K1's artifact; the
/// legacy code this test compares against is deleted in G4b.
fn code_review_pack_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("GENTS_CODE_REVIEW_PACK_DIR").unwrap_or_else(|_| {
            panic!(
                "GENTS_CODE_REVIEW_PACK_DIR must name a checkout of the code_review pack \
             (gents-ai/packs, packs/gents/code_review), built with its review_evidence plugin"
            )
        }),
    )
}

mod evidence_equivalence {
    use std::path::Path;

    fn init_repo() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(directory.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "equivalence@example.com"]);
        git(&["config", "user.name", "Equivalence"]);
        directory
    }

    fn commit(repo: &Path, message: &str) -> String {
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?} failed");
            output
        };
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", message]);
        String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned()
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }

    fn amend_commit(repo: &Path) -> String {
        git_stdout(repo, &["add", "-A"]);
        git_stdout(repo, &["commit", "--amend", "--quiet", "-m", "head"]);
        git_stdout(repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Whether the evidence packet (summary + patch, at the legacy adapter's
    /// unified=12/renames=50 settings) has byte 1800 land inside a multibyte
    /// character's encoding, mirroring `code_review_evidence`'s own
    /// assembly. Used only to search for content that exercises the
    /// straddle for real, not to assert equivalence (the outer test already
    /// compares the two implementations' actual output).
    fn evidence_packet_straddles_1800(repo: &Path, base: &str, head: &str) -> bool {
        let changed = git_stdout(repo, &["diff", "--name-status", base, head, "--"]);
        let stat = git_stdout(repo, &["diff", "--stat", base, head, "--"]);
        let patch = git_stdout(
            repo,
            &[
                "-c",
                "core.quotepath=true",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--find-renames=50%",
                "--unified=12",
                base,
                head,
                "--",
            ],
        );
        let summary = format!(
            "PINNED BASE: {base}\nPINNED HEAD: {head}\n\nCHANGED FILES:\n{}\n\nDIFF STAT:\n{}",
            changed.trim(),
            stat.trim()
        );
        let packet = format!("{summary}\n\nCOMPLETE PATCH:\n{patch}");
        packet.len() > 1800 && !packet.is_char_boundary(1800)
    }

    /// Six repos exercising the same shapes the packs-repo golden generator
    /// covers: an ASCII edit, a multibyte boundary, a rename plus a binary
    /// file, an empty diff, a large edit, and a non-ASCII filename.
    pub(super) fn cases() -> Vec<(&'static str, tempfile::TempDir, String, String)> {
        let mut cases = Vec::new();
        {
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("hello.txt"), "line one\n").unwrap();
            let base = commit(repo, "base");
            std::fs::write(repo.join("hello.txt"), "line one changed\n").unwrap();
            let head = commit(repo, "head");
            cases.push(("ascii-edit", directory, base, head));
        }
        {
            // A multibyte UTF-8 character positioned so its encoding
            // straddles byte 1800 of the evidence packet: the exact prefix
            // length before the patch content depends on git's own diff
            // header formatting, so this searches for the ASCII padding
            // that lands the straddle there rather than hand-computing it.
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("multibyte.txt"), "seed\n").unwrap();
            let base = commit(repo, "base");
            let mut head = String::new();
            let mut straddles = false;
            for pad in 0..64 {
                let content = format!(
                    "{}{}{}",
                    "a".repeat(200 + pad),
                    "日".repeat(3),
                    "é日".repeat(500)
                );
                std::fs::write(repo.join("multibyte.txt"), &content).unwrap();
                head = if pad == 0 {
                    commit(repo, "head")
                } else {
                    amend_commit(repo)
                };
                if evidence_packet_straddles_1800(repo, &base, &head) {
                    straddles = true;
                    break;
                }
            }
            assert!(
                straddles,
                "could not construct an evidence packet whose byte 1800 straddles a multibyte character"
            );
            cases.push(("multibyte-boundary", directory, base, head));
        }
        {
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("old_name.txt"), "a".repeat(200)).unwrap();
            let base = commit(repo, "base");
            std::fs::rename(repo.join("old_name.txt"), repo.join("new_name.txt")).unwrap();
            std::fs::write(repo.join("blob.bin"), [0u8, 159, 146, 150, 0, 255, 1, 2]).unwrap();
            let head = commit(repo, "head");
            cases.push(("rename-and-binary", directory, base, head));
        }
        {
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("unchanged.txt"), "steady\n").unwrap();
            let base = commit(repo, "base");
            cases.push(("empty-diff", directory, base.clone(), base));
        }
        {
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("large.txt"), "a\n".repeat(30_000)).unwrap();
            let base = commit(repo, "base");
            std::fs::write(repo.join("large.txt"), "b\n".repeat(60_000)).unwrap();
            let head = commit(repo, "head");
            cases.push(("large-edit", directory, base, head));
        }
        {
            let directory = init_repo();
            let repo = directory.path();
            std::fs::write(repo.join("日本語.txt"), "one\n").unwrap();
            let base = commit(repo, "base");
            std::fs::write(repo.join("日本語.txt"), "two\n").unwrap();
            let head = commit(repo, "head");
            cases.push(("non-ascii-filename", directory, base, head));
        }
        cases
    }
}

/// The six equivalence repos build without the real `review_evidence`
/// plugin, so a regression in their construction (in particular, the
/// multibyte case's search for a straddle at byte 1800) is caught without
/// needing `GENTS_CODE_REVIEW_PACK_DIR`.
#[tokio::test]
async fn evidence_equivalence_cases_build_six_distinct_repos() {
    let cases = evidence_equivalence::cases();
    assert_eq!(cases.len(), 6);
    let names: std::collections::BTreeSet<_> = cases.iter().map(|(name, ..)| *name).collect();
    assert_eq!(names.len(), 6, "case names must be distinct");
}

/// Proves the pack plugin reproduces the legacy Rust adapter's evidence
/// byte-for-byte: the same manifest and pages (every field, sorted by
/// `page_key`) and the same entry input, aside from the workspace facts each
/// independent `provision_read_only_workspace` call mints its own identity
/// for. `#[ignore]`d and env-gated on `GENTS_CODE_REVIEW_PACK_DIR`; deleted
/// in G4b along with the legacy adapter it compares against.
#[tokio::test]
#[ignore]
async fn graph_prepare_matches_legacy_code_review_evidence() {
    let pack_dir = code_review_pack_dir();
    let (bytes, _header) = gents::pack_archive::pack_dir(&pack_dir).expect("packing pack dir");
    let archive = gents::pack_archive::PackArchive::from_bytes(&bytes).expect("reading pack");
    let plugin_bytes = archive
        .plugin_artifact("review_evidence")
        .expect("review_evidence artifact")
        .to_vec();
    let digest = format!(
        "sha256:{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&plugin_bytes)
    );

    let home = tempfile::tempdir().unwrap();
    gents::plugin::store::store_bytes(
        home.path(),
        digest.strip_prefix("sha256:").unwrap(),
        &plugin_bytes,
    )
    .unwrap();
    let declaration = archive.plugin("review_evidence").expect("declared").clone();
    let record = gents::plugin::store::InstalledPlugin {
        namespace: "gents".into(),
        name: "review_evidence".into(),
        version: archive.manifest().version.clone(),
        digest: digest.clone(),
        language: declaration.language.clone(),
        declaration,
        granted: None,
        instructions: None,
        owner_pack_coordinate: None,
        owner_pack_digest: None,
    };
    gents::plugin::store::write_record(home.path(), &record).unwrap();
    let plugins = gents::plugin::executor::PluginExecutor::new(Some(home.path().to_owned()));

    // The pack's own compiled plan, not a hand-built stand-in: it already
    // carries the pinned `review_evidence` digest and the entry's real
    // `input_schema`/`prepare` (unified_context_lines, rename_similarity_percent,
    // the focus default), so a change to `pack_config.json` that this proof
    // must catch actually flows through it, rather than two copies of that
    // shape that could silently drift apart.
    let plan: gents::graph_pipeline::GraphPlan = serde_json::from_slice(
        archive
            .asset(&gents::graph_package::graph_plan_path("code-review"))
            .expect("pack ships graphs/code_review.plan.json"),
    )
    .expect("shipped plan parses");
    let pinned_digest = plan.entries[0]
        .prepare
        .as_ref()
        .and_then(|prepare| prepare.digest.as_deref());
    assert_eq!(
        pinned_digest,
        Some(digest.as_str()),
        "the shipped plan's pinned review_evidence digest must match the artifact this pack ships"
    );

    for (name, repo, base, head) in evidence_equivalence::cases() {
        let owner = "did:key:zEvidenceEquivalence";

        let manifest_schema =
            std::fs::read_to_string(pack_dir.join("schemas/evidence_manifest.graphql"))
                .expect("pack ships schemas/evidence_manifest.graphql");
        let page_schema = std::fs::read_to_string(pack_dir.join("schemas/evidence_page.graphql"))
            .expect("pack ships schemas/evidence_page.graphql");

        // Node A: the legacy Rust adapter.
        let node_a = defra_node::EmbeddedNode::builder().build().await.unwrap();
        gents::schema::ensure_runtime_schemas(&node_a)
            .await
            .unwrap();
        gents::document_config::ensure_agent_principal(&node_a, owner)
            .await
            .unwrap();
        node_a.add_schema(&manifest_schema).await.unwrap();
        node_a.add_schema(&page_schema).await.unwrap();
        let access_a = gents::config_client::ConfigAccess::Local(std::sync::Arc::new(node_a));
        let legacy = gents::graph_package::prepare_code_review_run(
            &access_a,
            owner,
            repo.path(),
            &base,
            &head,
            None,
            Some(repo.path()),
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: legacy prepare failed: {error:#}"));

        // Node B: the generic host step plus the pack's own plugin.
        let node_b = defra_node::EmbeddedNode::builder().build().await.unwrap();
        gents::schema::ensure_runtime_schemas(&node_b)
            .await
            .unwrap();
        gents::document_config::ensure_agent_principal(&node_b, owner)
            .await
            .unwrap();
        node_b.add_schema(&manifest_schema).await.unwrap();
        node_b.add_schema(&page_schema).await.unwrap();
        let access_b = gents::config_client::ConfigAccess::Local(std::sync::Arc::new(node_b));
        let prepared = gents::graph_package::prepare_entry_run(
            &access_b,
            owner,
            gents::graph_package::EntryRunRequest {
                plan: &plan,
                entry: None,
                input: serde_json::json!({
                    "repository": repo.path().to_string_lossy(),
                    "base": base,
                    "head": head,
                }),
                host_root: Some(repo.path()),
                plugins: &plugins,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: generic prepare failed: {error:#}"));

        let legacy_evidence_id = legacy.input["evidence_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: legacy input missing evidence_id"))
            .to_owned();
        let generic_evidence_id = prepared.input["evidence_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: generic input missing evidence_id"))
            .to_owned();

        let mut legacy_input = legacy.input.clone();
        let mut generic_input = prepared.input.clone();
        for stripped in ["workspace_id", "workspace_owner_agent_did", "evidence_id"] {
            // The evidence_id (nonce) and the workspace identity are minted
            // fresh per call by design; every other field must match exactly.
            legacy_input.as_object_mut().unwrap().remove(stripped);
            generic_input.as_object_mut().unwrap().remove(stripped);
        }
        assert_eq!(legacy_input, generic_input, "{name}: entry input");

        let manifests_a = access_a
            .execute("{ CodeReviewEvidenceManifest { evidence_id format_version page_count evidence_chunk_count evidence_byte_count evidence_sha256 } }")
            .await
            .unwrap();
        let manifests_b = access_b
            .execute("{ CodeReviewEvidenceManifest { evidence_id format_version page_count evidence_chunk_count evidence_byte_count evidence_sha256 } }")
            .await
            .unwrap();
        let manifest_a = &manifests_a["data"]["CodeReviewEvidenceManifest"][0];
        let manifest_b = &manifests_b["data"]["CodeReviewEvidenceManifest"][0];
        // A plugin that got the nonce linkage wrong (e.g. minted its own
        // evidence_id instead of using the one on its stdin) would still
        // pass a comparison that only strips these fields; check each
        // side's manifest actually carries the evidence_id its own prepared
        // input names.
        assert_eq!(
            manifest_a["evidence_id"], legacy_evidence_id,
            "{name}: legacy manifest evidence_id must equal the legacy input's evidence_id"
        );
        assert_eq!(
            manifest_b["evidence_id"], generic_evidence_id,
            "{name}: generic manifest evidence_id must equal the generic input's evidence_id"
        );
        let strip_evidence_id = |value: &Value| {
            let mut value = value.clone();
            value.as_object_mut().unwrap().remove("evidence_id");
            value
        };
        assert_eq!(
            strip_evidence_id(manifest_a),
            strip_evidence_id(manifest_b),
            "{name}: manifest"
        );

        // `page_key` embeds the nonce, which each side mints independently;
        // every other field, including all sixteen chunk slots, is the
        // byte-identity proof and must match exactly.
        let chunk_fields: String = (0..16)
            .map(|slot| format!("evidence_chunk_{slot} "))
            .collect();
        let page_query = format!(
            "{{ CodeReviewEvidencePage {{ page_key evidence_id page_index page_count \
             evidence_chunk_count evidence_byte_count evidence_sha256 {chunk_fields}}} }}"
        );
        let mut pages_a = access_a.execute(&page_query).await.unwrap()["data"]
            ["CodeReviewEvidencePage"]
            .as_array()
            .unwrap()
            .clone();
        let mut pages_b = access_b.execute(&page_query).await.unwrap()["data"]
            ["CodeReviewEvidencePage"]
            .as_array()
            .unwrap()
            .clone();
        let by_page_index = |value: &Value| {
            value["page_index"]
                .as_str()
                .unwrap()
                .parse::<u32>()
                .unwrap()
        };
        pages_a.sort_by_key(by_page_index);
        pages_b.sort_by_key(by_page_index);
        for (side, pages, evidence_id) in [
            ("legacy", &pages_a, &legacy_evidence_id),
            ("generic", &pages_b, &generic_evidence_id),
        ] {
            for page in pages {
                let page_index = page["page_index"].as_str().unwrap();
                assert_eq!(
                    page["evidence_id"].as_str().unwrap(),
                    evidence_id.as_str(),
                    "{name}: {side} page {page_index} evidence_id"
                );
                assert_eq!(
                    page["page_key"].as_str().unwrap(),
                    format!("{evidence_id}:{page_index:0>8}"),
                    "{name}: {side} page {page_index} page_key"
                );
            }
        }
        let strip_page_identity = |pages: &[Value]| {
            pages
                .iter()
                .map(|page| {
                    let mut page = page.clone();
                    let object = page.as_object_mut().unwrap();
                    object.remove("page_key");
                    object.remove("evidence_id");
                    page
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            strip_page_identity(&pages_a),
            strip_page_identity(&pages_b),
            "{name}: pages"
        );
    }
}
