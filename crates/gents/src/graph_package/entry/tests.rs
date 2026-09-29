use super::*;

#[test]
fn review_cannot_escape_ceiling_through_enclosing_repository() {
    let directory = tempfile::tempdir().unwrap();
    git_output(directory.path(), &["init", "--quiet"]).unwrap();
    let permitted = directory.path().join("permitted");
    std::fs::create_dir(&permitted).unwrap();
    let permitted = std::fs::canonicalize(permitted).unwrap();
    let error = resolve_repository(&permitted, "HEAD", "HEAD", Some(&permitted)).unwrap_err();
    assert!(
        error.to_string().contains("escapes operator tool root"),
        "{error:#}"
    );
}

#[test]
fn evidence_pages_are_complete_and_bounded() {
    let packet = format!("{}{}", "a".repeat(1_750_000), "é日".repeat(2_000));
    let chunks = split_evidence_packet(&packet);
    assert_eq!(chunks.concat(), packet);
    assert!(chunks
        .iter()
        .all(|chunk| chunk.len() <= EVIDENCE_CHUNK_MAX_BYTES));
    assert!(EVIDENCE_CHUNK_MAX_BYTES < 2_000);
    let pages = evidence_page_inputs("evidence", "digest", packet.len(), &chunks);
    assert_eq!(pages.len(), chunks.len().div_ceil(EVIDENCE_CHUNKS_PER_PAGE));
    assert!(
        pages.len() > 18,
        "evidence paging must not reintroduce a fixed patch-size ceiling"
    );
    let mut reconstructed = Vec::new();
    for (page, input) in pages.iter().enumerate() {
        let input = input.as_object().unwrap();
        assert_eq!(input.len(), EVIDENCE_CHUNKS_PER_PAGE + 7);
        assert_eq!(input["page_key"], format!("evidence:{page:08}"));
        assert_eq!(input["page_index"], page.to_string());
        assert_eq!(input["page_count"], pages.len().to_string());
        assert_eq!(input["evidence_chunk_count"], chunks.len().to_string());
        assert_eq!(input["evidence_byte_count"], packet.len().to_string());
        assert!(serde_json::to_vec(input).unwrap().len() < 50 * 1024);
        for slot in 0..EVIDENCE_CHUNKS_PER_PAGE {
            let chunk = page * EVIDENCE_CHUNKS_PER_PAGE + slot;
            let value = input[&format!("evidence_chunk_{slot}")].as_str().unwrap();
            if chunk < chunks.len() {
                reconstructed.push(value.to_owned());
            } else {
                assert!(value.is_empty(), "only final page padding may be empty");
            }
        }
    }
    assert_eq!(reconstructed.concat(), packet);
}

#[test]
fn empty_evidence_packet_has_no_rows() {
    let chunks = split_evidence_packet("");
    assert!(chunks.is_empty());
    assert!(evidence_page_inputs("empty", "digest", 0, &chunks).is_empty());
}

fn init_repo() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    git_output(directory.path(), &["init", "--quiet"]).unwrap();
    git_output(
        directory.path(),
        &["config", "user.email", "test@example.com"],
    )
    .unwrap();
    git_output(directory.path(), &["config", "user.name", "Test"]).unwrap();
    directory
}

fn commit(repo: &Path, message: &str) -> String {
    git_output(repo, &["add", "-A"]).unwrap();
    git_output(repo, &["commit", "--quiet", "-m", message]).unwrap();
    git_output(repo, &["rev-parse", "HEAD"]).unwrap()
}

#[test]
fn git_diff_host_step_runs_the_declared_diff() {
    let directory = init_repo();
    let repo = directory.path();
    std::fs::write(repo.join("a.txt"), "line one\n").unwrap();
    let base = commit(repo, "base");
    std::fs::write(repo.join("a.txt"), "line one changed\n").unwrap();
    let head = commit(repo, "head");

    let facts = collect_git_diff(repo, &base, &head, 3, 40, None).unwrap();
    let expected_name_status =
        git_output(repo, &["diff", "--name-status", &base, &head, "--"]).unwrap();
    let expected_stat = git_output(repo, &["diff", "--stat", &base, &head, "--"]).unwrap();
    let expected_patch = git_output_exact(
        repo,
        &[
            "-c",
            "core.quotepath=true",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--find-renames=40%",
            "--unified=3",
            &base,
            &head,
            "--",
        ],
    )
    .unwrap();
    assert_eq!(facts.base_sha, base);
    assert_eq!(facts.head_sha, head);
    assert_eq!(facts.name_status, expected_name_status);
    assert_eq!(facts.stat, expected_stat);
    assert_eq!(facts.patch, expected_patch);
}

/// A guest that ignores stdin and writes exactly `json` to stdout: the
/// prepare plugin fixture for tests that only need a deterministic
/// result, not a real transformation of the host facts.
fn constant_output_wat(json: &[u8]) -> String {
    let mut escaped = String::with_capacity(json.len() * 4);
    for byte in json {
        escaped.push_str(&format!("\\{byte:02x}"));
    }
    let len = json.len();
    format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
(func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{escaped}")
  (func (export "_start")
(i32.store (i32.const 8192) (i32.const 0))
(i32.store (i32.const 8196) (i32.const {len}))
(call $fd_write (i32.const 1) (i32.const 8192) (i32.const 1) (i32.const 8200))
drop))
"#
    )
}

/// Installs a constant-output plugin under a fresh home, qualified
/// `fixture/prepare_fixture`.
fn install_prepare_fixture_plugin(output: &Value) -> (tempfile::TempDir, String) {
    let wat_source = constant_output_wat(&serde_json::to_vec(output).unwrap());
    let (plugin, afb) =
        crate::plugin::tests::build_plugin_pack("prepare_fixture_pack", &wat_source, None);
    let home = tempfile::tempdir().unwrap();
    let hex = format!("{:x}", Sha256::digest(&afb));
    crate::plugin::store::store_bytes(home.path(), &hex, &afb).unwrap();
    let digest = format!("sha256:{hex}");
    let record = crate::plugin::store::InstalledPlugin {
        namespace: "fixture".into(),
        name: plugin.name.clone(),
        version: "0.1.0".into(),
        digest: digest.clone(),
        language: "rust".into(),
        declaration: plugin,
        granted: None,
        instructions: None,
        owner_pack_coordinate: None,
        owner_pack_digest: None,
    };
    crate::plugin::store::write_record(home.path(), &record).unwrap();
    (home, digest)
}

fn fixture_entry(prepare: Option<crate::graph_pipeline::EntryPrepare>) -> PlannedEntry {
    PlannedEntry {
        name: "job".to_owned(),
        collection: "FixtureJob".to_owned(),
        schema: "FixtureJob/v1".to_owned(),
        input_contract: None,
        to: crate::graph_pipeline::PortRef {
            node_id: "worker".to_owned(),
            port: "job".to_owned(),
        },
        target: crate::graph_pipeline::StageTarget::Task {
            task_id: "worker-task".to_owned(),
        },
        correlation_field: "correlation".to_owned(),
        input_schema: None,
        prepare,
    }
}

fn fixture_plan(entry: PlannedEntry) -> GraphPlan {
    GraphPlan {
        compiler_version: crate::graph_pipeline::COMPILER_VERSION.to_owned(),
        graph_id: "fixture-graph".to_owned(),
        digest: format!("sha256:{}", "0".repeat(64)),
        nodes: Vec::new(),
        edges: Vec::new(),
        entries: vec![entry],
        results: Vec::new(),
        capability_manifest: Vec::new(),
        limits: crate::graph_pipeline::GraphLimits {
            max_nodes: 1,
            max_edges: 1,
            max_depth: 1,
            max_fan_out: 1,
            max_total_invocations: 1,
            max_runtime_secs: 60,
        },
        package: None,
    }
}

#[tokio::test]
async fn prepare_entry_run_returns_admitted_input_when_the_entry_has_no_prepare() {
    let plan = fixture_plan(fixture_entry(None));
    let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
    let access = ConfigAccess::Local(std::sync::Arc::new(node));
    let plugins = PluginExecutor::default();
    let prepared = prepare_entry_run(
        &access,
        "did:key:tester",
        EntryRunRequest {
            plan: &plan,
            entry: None,
            input: json!({"note": "hello"}),
            host_root: None,
            plugins: &plugins,
        },
    )
    .await
    .unwrap();
    assert_eq!(prepared.origin, EntryInputOrigin::Operator);
    assert_eq!(prepared.documents, 0);
    assert_eq!(prepared.input, json!({"note": "hello"}));
}

#[tokio::test]
async fn prepare_entry_run_runs_host_steps_and_persists_the_plugins_documents() {
    let directory = init_repo();
    let repo = directory.path();
    std::fs::write(repo.join("a.txt"), "line one\n").unwrap();
    let base = commit(repo, "base");
    std::fs::write(repo.join("a.txt"), "line one changed\n").unwrap();
    let head = commit(repo, "head");

    let plugin_output = json!({
        "input": {"summary": "prepared"},
        "documents": [{"collection": "FixtureEvidence", "fields": {"note": "from the plugin"}}],
    });
    let (home, digest) = install_prepare_fixture_plugin(&plugin_output);
    let plugins = PluginExecutor::new(Some(home.path().to_owned()));

    let entry = fixture_entry(Some(crate::graph_pipeline::EntryPrepare {
        host: vec![HostInput::GitDiff {
            repository_field: "repository".to_owned(),
            base_field: "base".to_owned(),
            head_field: "head".to_owned(),
            unified_context_lines: 3,
            rename_similarity_percent: 50,
        }],
        plugin: "fixture/plugin".to_owned(),
        digest: Some(digest),
        writes: vec!["FixtureEvidence".to_owned()],
    }));
    let plan = fixture_plan(entry);

    let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
    node.add_schema("type FixtureEvidence { note: String }")
        .await
        .unwrap();
    let access = ConfigAccess::Local(std::sync::Arc::new(node));

    let prepared = prepare_entry_run(
        &access,
        "did:key:tester",
        EntryRunRequest {
            plan: &plan,
            entry: None,
            input: json!({
                "repository": repo.to_string_lossy(),
                "base": base,
                "head": head,
            }),
            host_root: None,
            plugins: &plugins,
        },
    )
    .await
    .unwrap();

    assert_eq!(prepared.origin, EntryInputOrigin::Prepared);
    assert_eq!(prepared.documents, 1);
    assert_eq!(prepared.input, json!({"summary": "prepared"}));

    let response = access
        .execute("{ FixtureEvidence { note } }")
        .await
        .unwrap();
    let notes: Vec<&str> = response["data"]["FixtureEvidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["note"].as_str().unwrap())
        .collect();
    assert_eq!(notes, vec!["from the plugin"]);
}

#[tokio::test]
async fn prepare_entry_run_refuses_a_document_outside_declared_writes() {
    let directory = init_repo();
    let repo = directory.path();
    std::fs::write(repo.join("a.txt"), "x\n").unwrap();
    let base = commit(repo, "base");
    std::fs::write(repo.join("a.txt"), "y\n").unwrap();
    let head = commit(repo, "head");

    let plugin_output = json!({
        "input": {},
        "documents": [{"collection": "NotDeclared", "fields": {}}],
    });
    let (home, digest) = install_prepare_fixture_plugin(&plugin_output);
    let plugins = PluginExecutor::new(Some(home.path().to_owned()));
    let entry = fixture_entry(Some(crate::graph_pipeline::EntryPrepare {
        host: vec![HostInput::GitDiff {
            repository_field: "repository".to_owned(),
            base_field: "base".to_owned(),
            head_field: "head".to_owned(),
            unified_context_lines: 3,
            rename_similarity_percent: 50,
        }],
        plugin: "fixture/plugin".to_owned(),
        digest: Some(digest),
        writes: vec!["FixtureEvidence".to_owned()],
    }));
    let plan = fixture_plan(entry);
    let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
    let access = ConfigAccess::Local(std::sync::Arc::new(node));
    let error = prepare_entry_run(
        &access,
        "did:key:tester",
        EntryRunRequest {
            plan: &plan,
            entry: None,
            input: json!({"repository": repo.to_string_lossy(), "base": base, "head": head}),
            host_root: None,
            plugins: &plugins,
        },
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("does not declare in prepare.writes"),
        "{error:#}"
    );
}

/// The batched aliased-mutation path (D4 step 7): more than
/// [`PREPARE_DOCUMENTS_BATCH_LIMIT`] documents forces the count-based split
/// into multiple batches, and one oversized document forces a byte-based
/// split mid-batch (and must still land, alone, in its own batch rather than
/// being silently dropped). Every document must survive with its exact
/// fields, and none may be dropped, duplicated, or truncated.
#[tokio::test]
async fn persist_prepared_documents_batches_and_never_drops_a_document() {
    let node = defra_node::EmbeddedNode::builder().build().await.unwrap();
    node.add_schema("type FixtureEvidence { note: String }")
        .await
        .unwrap();
    let access = ConfigAccess::Local(std::sync::Arc::new(node));

    let mut documents: Vec<PreparePluginDocument> = (0..70)
        .map(|i| PreparePluginDocument {
            collection: "FixtureEvidence".to_owned(),
            fields: json!({"note": format!("doc-{i:03}")}),
        })
        .collect();
    let oversized_note = "z".repeat(PREPARE_DOCUMENTS_BATCH_BYTES + 1);
    // Inserted mid-list (not first in its would-be batch), so this document
    // alone tripping the byte budget forces an early batch break rather than
    // only ever being exercised as a batch's first, always-admitted alias.
    documents.insert(
        40,
        PreparePluginDocument {
            collection: "FixtureEvidence".to_owned(),
            fields: json!({"note": oversized_note.clone()}),
        },
    );
    assert!(
        documents.len() > 2 * PREPARE_DOCUMENTS_BATCH_LIMIT,
        "the count-based split must be exercised more than once"
    );

    persist_prepared_documents(&access, &documents)
        .await
        .unwrap();

    let response = access
        .execute("{ FixtureEvidence { note } }")
        .await
        .unwrap();
    let mut notes: Vec<String> = response["data"]["FixtureEvidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["note"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        notes.len(),
        documents.len(),
        "every document must persist exactly once"
    );
    assert!(
        notes.contains(&oversized_note),
        "the oversized document must not be dropped or truncated"
    );
    notes.retain(|note| note != &oversized_note);
    let mut expected: Vec<String> = (0..70).map(|i| format!("doc-{i:03}")).collect();
    notes.sort();
    expected.sort();
    assert_eq!(notes, expected, "every small document must survive intact");
}

/// Fixed facts the golden generator and the legacy/plugin equivalence
/// test both pin, so the recorded fixtures are reproducible and the two
/// implementations are compared over identical inputs.
const GOLDEN_NONCE: &str = "00000000-0000-0000-0000-0000000000e1";
const GOLDEN_WORKSPACE_ID: &str = "00000000-0000-0000-0000-0000000000w5";
const GOLDEN_WORKSPACE_OWNER: &str = "did:key:golden-owner";
const GOLDEN_FOCUS: &str =
    "Review the diff for material correctness, safety, durability, and maintainability defects.";

struct GoldenCase {
    name: &'static str,
    repo: tempfile::TempDir,
    base: String,
    head: String,
}

fn write_binary(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

fn golden_cases() -> Vec<GoldenCase> {
    let mut cases = Vec::new();

    // 1. an ASCII edit.
    {
        let directory = init_repo();
        let repo = directory.path();
        std::fs::write(repo.join("hello.txt"), "line one\n").unwrap();
        let base = commit(repo, "base");
        std::fs::write(repo.join("hello.txt"), "line one changed\n").unwrap();
        let head = commit(repo, "head");
        cases.push(GoldenCase {
            name: "ascii-edit",
            repo: directory,
            base,
            head,
        });
    }

    // 2. a multibyte UTF-8 character positioned so its encoding straddles
    // byte 1800 of the assembled evidence packet (summary + patch, at the
    // golden settings unified=12/renames=50): the exact prefix length before
    // the patch content depends on git's own diff-header formatting, so this
    // searches for the ASCII padding that lands the straddle there instead
    // of hand-computing it, and fails loud if none of the tried paddings do.
    {
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
            git_output(repo, &["add", "-A"]).unwrap();
            if pad == 0 {
                git_output(repo, &["commit", "--quiet", "-m", "head"]).unwrap();
            } else {
                git_output(repo, &["commit", "--amend", "--quiet", "-m", "head"]).unwrap();
            }
            head = git_output(repo, &["rev-parse", "HEAD"]).unwrap();
            let facts = collect_git_diff(repo, &base, &head, 12, 50, None).unwrap();
            let evidence = code_review_evidence(repo, &facts.base_sha, &facts.head_sha).unwrap();
            let packet: String = evidence.chunks.concat();
            if packet.len() > 1800 && !packet.is_char_boundary(1800) {
                straddles = true;
                break;
            }
        }
        assert!(
            straddles,
            "could not construct an evidence packet whose byte 1800 straddles a multibyte character"
        );
        cases.push(GoldenCase {
            name: "multibyte-boundary",
            repo: directory,
            base,
            head,
        });
    }

    // 3. a rename plus a binary file.
    {
        let directory = init_repo();
        let repo = directory.path();
        std::fs::write(repo.join("old_name.txt"), "a".repeat(200)).unwrap();
        let base = commit(repo, "base");
        std::fs::rename(repo.join("old_name.txt"), repo.join("new_name.txt")).unwrap();
        write_binary(&repo.join("blob.bin"), &[0u8, 159, 146, 150, 0, 255, 1, 2]);
        let head = commit(repo, "head");
        cases.push(GoldenCase {
            name: "rename-and-binary",
            repo: directory,
            base,
            head,
        });
    }

    // 4. base == head (empty diff).
    {
        let directory = init_repo();
        let repo = directory.path();
        std::fs::write(repo.join("unchanged.txt"), "steady\n").unwrap();
        let base = commit(repo, "base");
        cases.push(GoldenCase {
            name: "empty-diff",
            head: base.clone(),
            repo: directory,
            base,
        });
    }

    // 5. an edit of about 120 KB.
    {
        let directory = init_repo();
        let repo = directory.path();
        std::fs::write(repo.join("large.txt"), "a\n".repeat(30_000)).unwrap();
        let base = commit(repo, "base");
        std::fs::write(repo.join("large.txt"), "b\n".repeat(60_000)).unwrap();
        let head = commit(repo, "head");
        cases.push(GoldenCase {
            name: "large-edit",
            repo: directory,
            base,
            head,
        });
    }

    // 6. a non-ASCII filename, exercising core.quotepath.
    {
        let directory = init_repo();
        let repo = directory.path();
        std::fs::write(repo.join("日本語.txt"), "one\n").unwrap();
        let base = commit(repo, "base");
        std::fs::write(repo.join("日本語.txt"), "two\n").unwrap();
        let head = commit(repo, "head");
        cases.push(GoldenCase {
            name: "non-ascii-filename",
            repo: directory,
            base,
            head,
        });
    }

    cases
}

/// Generates the byte-identity goldens for the packs-repo `review_evidence`
/// plugin while the legacy Rust evidence code this file still carries is
/// the reference implementation. `#[ignore]`d: run explicitly with
/// `GENTS_REVIEW_EVIDENCE_GOLDEN_OUT` set to the plugin's `tests/` dir.
#[test]
#[ignore]
fn legacy_evidence_goldens() {
    let out_dir = std::env::var("GENTS_REVIEW_EVIDENCE_GOLDEN_OUT")
        .expect("set GENTS_REVIEW_EVIDENCE_GOLDEN_OUT to the plugin's tests/ directory");
    let out_dir = Path::new(&out_dir);
    std::fs::create_dir_all(out_dir).unwrap();
    for (index, case) in golden_cases().into_iter().enumerate() {
        let repo = case.repo.path();
        let facts = collect_git_diff(repo, &case.base, &case.head, 12, 50, None).unwrap();
        let evidence = code_review_evidence(repo, &facts.base_sha, &facts.head_sha).unwrap();
        if case.name == "multibyte-boundary" {
            // The case construction already searched for this property;
            // re-check it here so the golden generator itself, not only the
            // search that built the repo, would fail loud if it regressed.
            let packet: String = evidence.chunks.concat();
            assert!(
                !packet.is_char_boundary(1800),
                "golden case 2 must straddle byte 1800 of the evidence packet"
            );
            assert!(
                evidence.chunks[0].len() < EVIDENCE_CHUNK_MAX_BYTES,
                "chunk_0 must back off before the multibyte boundary"
            );
        }
        let manifest = json!({
            "evidence_id": GOLDEN_NONCE,
            "format_version": "1",
            "page_count": evidence.chunks.len().div_ceil(EVIDENCE_CHUNKS_PER_PAGE).to_string(),
            "evidence_chunk_count": evidence.chunks.len().to_string(),
            "evidence_byte_count": evidence.byte_count.to_string(),
            "evidence_sha256": evidence.sha256,
        });
        let pages = evidence_page_inputs(
            GOLDEN_NONCE,
            &evidence.sha256,
            evidence.byte_count,
            &evidence.chunks,
        );
        let mut documents = vec![PreparePluginDocument {
            collection: "CodeReviewEvidenceManifest".to_owned(),
            fields: manifest,
        }];
        documents.extend(pages.into_iter().map(|fields| PreparePluginDocument {
            collection: "CodeReviewEvidencePage".to_owned(),
            fields,
        }));

        let plugin_input = prepare_stdin(
            json!({
                "repository": facts.repository.to_string_lossy(),
                "base": facts.base_sha,
                "head": facts.head_sha,
                "focus": GOLDEN_FOCUS,
            }),
            GOLDEN_NONCE,
            json!({
                "git_diff": git_diff_host_json(&facts),
                "workspace": read_only_workspace_host_json(
                    GOLDEN_WORKSPACE_ID,
                    GOLDEN_WORKSPACE_OWNER,
                ),
            }),
        );
        let expect = PreparePluginOutput {
            input: json!({
                "repository_path": ".",
                "base_ref": facts.base_sha,
                "head_ref": facts.head_sha,
                "workspace_id": GOLDEN_WORKSPACE_ID,
                "workspace_authority": "readOnly",
                "workspace_owner_agent_did": GOLDEN_WORKSPACE_OWNER,
                "lens_count": "4",
                "lens_min": "4",
                "lens_max": "4",
                "pr_number": "",
                "evidence_id": GOLDEN_NONCE,
                "evidence_summary": evidence.summary,
                "evidence_chunk_count": evidence.chunks.len().to_string(),
                "focus": GOLDEN_FOCUS,
            }),
            documents,
        };
        let expect = serde_json::to_value(&expect).unwrap();
        // The runtime contract, proven directly: a golden a real plugin can
        // satisfy also parses as what `prepare_entry_run` deserializes.
        serde_json::from_value::<PreparePluginOutput>(expect.clone()).unwrap_or_else(|error| {
            panic!(
                "{}: golden expect must parse as PreparePluginOutput: {error}",
                case.name
            )
        });
        let golden = json!({"input": plugin_input, "expect": expect});
        let path = out_dir.join(format!("{:02}-{}.json", index + 1, case.name));
        std::fs::write(&path, serde_json::to_vec_pretty(&golden).unwrap()).unwrap();
    }
}
