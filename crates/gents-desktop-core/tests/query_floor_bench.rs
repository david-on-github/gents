//! Timing harness over a copy of a real desktop store.  Ignored by default;
//! run with `GENTS_BENCH_DESKTOP_ROOT=<copied desktop root>
//! GENTS_BENCH_SESSION=<session id> cargo test -p gents-desktop-core
//! --test query_floor_bench -- --ignored --nocapture`.  The root must be a
//! copy: the harness opens it as the desktop would, with P2P bound to
//! localhost only, and never against the live store.
use std::time::{Duration, Instant};

use anyhow::Result;
use gents::graphql::escape_graphql_string;
use gents_desktop_core::client::{
    load_session_transcript_page, ClientCore, ClientCoreOptions, DesktopPaths,
};
use serde_json::Value;

async fn timed<F, Fut, T>(label: &str, rounds: usize, mut run: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut samples = Vec::with_capacity(rounds);
    let mut last = None;
    for _ in 0..rounds {
        let started = Instant::now();
        last = Some(run().await?);
        samples.push(started.elapsed());
    }
    samples.sort();
    let min = samples[0];
    let median = samples[samples.len() / 2];
    eprintln!(
        "{label:<44} min {:>7.1} ms   median {:>7.1} ms",
        as_ms(min),
        as_ms(median)
    );
    Ok(last.expect("at least one round"))
}

fn as_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn rows<'a>(data: &'a Value, root: &str) -> &'a Vec<Value> {
    data[root].as_array().expect("array root")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing harness over a copied desktop store"]
async fn query_floor_over_copied_store() -> Result<()> {
    let root = std::env::var("GENTS_BENCH_DESKTOP_ROOT")?;
    let session_id = std::env::var("GENTS_BENCH_SESSION")?;
    let options = match std::env::var("GENTS_BENCH_P2P").as_deref() {
        Ok("full") => ClientCoreOptions::default(),
        _ => ClientCoreOptions::local_only(),
    };
    eprintln!("p2p options: {options:?}");
    let core = ClientCore::start_with_paths_and_options(
        DesktopPaths::from_root(std::path::Path::new(&root)),
        options,
    )
    .await?;
    if let Some(secs) = std::env::var("GENTS_BENCH_SETTLE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        eprintln!("settling {secs}s for peer connection and replication");
        tokio::time::sleep(Duration::from_secs(secs)).await;
        eprintln!("peers: {:?}", core.sync_state().peers);
    }
    let node = core.node();
    let query = |q: String| async move {
        gents::graphql::graphql_with_transaction_retry(node, &q, "bench")
            .await?
            .data
            .ok_or_else(|| anyhow::anyhow!("no data"))
    };
    let session = escape_graphql_string(&session_id);

    let sample = query(format!(
        r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID agent_did requester_did }} }}"#
    ))
    .await?;
    let segments = rows(&sample, "AgentOutputSegment").clone();
    anyhow::ensure!(!segments.is_empty(), "session has no output segments");
    let agent = segments[0]["agent_did"].as_str().unwrap().to_string();
    let requester = segments[0]["requester_did"].as_str().unwrap().to_string();
    let first_id = segments[0]["_docID"].as_str().unwrap().to_string();
    let in_clause = segments
        .iter()
        .map(|row| format!("\"{}\"", row["_docID"].as_str().unwrap()))
        .collect::<Vec<_>>()
        .join(", ");
    let scope = format!(
        r#"agent_did: {{ _eq: "{}" }}, requester_did: {{ _eq: "{}" }}"#,
        escape_graphql_string(&agent),
        escape_graphql_string(&requester)
    );
    eprintln!(
        "store {root}\nsession {session_id}\nsegments sampled {}\n",
        segments.len()
    );

    timed("trivial: AgentSession limit 1", 7, || {
        query("{ AgentSession(limit: 1) { _docID } }".to_string())
    })
    .await?;
    timed("segment by _docID, unscoped", 7, || {
        query(format!(
            r#"{{ AgentOutputSegment(filter: {{ _docID: {{ _eq: "{first_id}" }} }}, limit: 2) {{ _docID ordinal }} }}"#
        ))
    })
    .await?;
    timed("segment by _docID, scoped", 7, || {
        query(format!(
            r#"{{ AgentOutputSegment(filter: {{ _docID: {{ _eq: "{first_id}" }}, {scope} }}, limit: 2) {{ _docID ordinal }} }}"#
        ))
    })
    .await?;
    let batched = timed(
        &format!("segments by _docID _in [{}], scoped", segments.len()),
        7,
        || {
            query(format!(
                r#"{{ AgentOutputSegment(filter: {{ _docID: {{ _in: [{in_clause}] }}, {scope} }}, limit: 200) {{ _docID ordinal }} }}"#
            ))
        },
    )
    .await?;
    eprintln!(
        "    batched rows returned: {}",
        rows(&batched, "AgentOutputSegment").len()
    );
    timed("segments by session_id (indexed), 40", 7, || {
        query(format!(
            r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID ordinal }} }}"#
        ))
    })
    .await?;

    let page = timed("load_session_transcript_page: tip (40)", 5, || {
        let (session_id, agent, requester) = (&session_id, &agent, &requester);
        async move {
            load_session_transcript_page(
                node,
                session_id,
                Some(agent),
                Some(requester),
                None,
                Some(40),
            )
            .await
        }
    })
    .await?;
    eprintln!(
        "    tip queries {} rows {} messages {} tool calls {}",
        page.query_count,
        page.queried_rows,
        page.store.transcript_messages.len(),
        page.store.tool_calls.len()
    );
    let oldest = page
        .store
        .transcript_messages
        .iter()
        .min_by_key(|row| row.message.sequence)
        .map(|row| row.message.message_key.clone());
    if let Some(cursor) = oldest {
        let session_id = &session_id;
        let agent = &agent;
        let requester = &requester;
        let cursor = &cursor;
        let older = timed(
            "load_session_transcript_page: older (40)",
            5,
            || async move {
                load_session_transcript_page(
                    node,
                    session_id,
                    Some(agent),
                    Some(requester),
                    Some(cursor),
                    Some(40),
                )
                .await
            },
        )
        .await?;
        eprintln!(
            "    older queries {} rows {} messages {} tool calls {}",
            older.query_count,
            older.queried_rows,
            older.store.transcript_messages.len(),
            older.store.tool_calls.len()
        );
    }
    Ok(())
}

/// Same floor probe over any copied DefraDB store opened without a desktop
/// identity: `GENTS_BENCH_STORE=<node data dir> GENTS_BENCH_SESSION=<id>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing harness over a copied DefraDB store"]
async fn query_floor_over_raw_store() -> Result<()> {
    use defra_node::{NodeBuilder, StorageBackend};
    let store = std::env::var("GENTS_BENCH_STORE")?;
    let session_id = std::env::var("GENTS_BENCH_SESSION")?;
    let mut builder = NodeBuilder::default()
        .data_path(std::path::PathBuf::from(&store))
        .with_storage_backend(StorageBackend::Regolith);
    if let Ok(did) = std::env::var("GENTS_BENCH_NODE_DID") {
        builder = builder.with_node_identity_did(did);
    }
    let node = builder.build().await?;
    let node = &node;
    let query = |q: String| async move {
        gents::graphql::graphql_with_transaction_retry(node, &q, "bench")
            .await?
            .data
            .ok_or_else(|| anyhow::anyhow!("no data"))
    };
    let session = escape_graphql_string(&session_id);
    let sample = query(format!(
        r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID }} }}"#
    ))
    .await?;
    let segments = rows(&sample, "AgentOutputSegment").clone();
    eprintln!(
        "store {store}\nsession {session_id}\nsegments visible {}\n",
        segments.len()
    );
    timed("trivial: AgentSession limit 1", 7, || {
        query("{ AgentSession(limit: 1) { _docID } }".to_string())
    })
    .await?;
    if let Some(first) = segments.first() {
        let first_id = first["_docID"].as_str().unwrap().to_string();
        timed("segment by _docID, unscoped", 7, || {
            query(format!(
                r#"{{ AgentOutputSegment(filter: {{ _docID: {{ _eq: "{first_id}" }} }}, limit: 2) {{ _docID ordinal }} }}"#
            ))
        })
        .await?;
    }
    timed("segments by session_id (indexed), 40", 7, || {
        query(format!(
            r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID ordinal }} }}"#
        ))
    })
    .await?;
    timed("messages by session_id (indexed), 40", 7, || {
        query(format!(
            r#"{{ AgentMessage(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID sequence }} }}"#
        ))
    })
    .await?;
    Ok(())
}

/// Does in-process write volume degrade unrelated reads, and does a reopen
/// restore them?  Mutates the store, so point it at a throwaway copy:
/// `GENTS_BENCH_DESKTOP_ROOT=<copy> GENTS_BENCH_SESSION=<id> GENTS_BENCH_WRITES=400`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mutating timing harness over a throwaway desktop store copy"]
async fn reads_after_in_process_writes_and_after_reopen() -> Result<()> {
    let root = std::env::var("GENTS_BENCH_DESKTOP_ROOT")?;
    let session_id = std::env::var("GENTS_BENCH_SESSION")?;
    let writes: usize = std::env::var("GENTS_BENCH_WRITES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(400);
    let paths = || DesktopPaths::from_root(std::path::Path::new(&root));

    async fn probe(core: &ClientCore, session_id: &str, label: &str) -> Result<()> {
        let node = core.node();
        let session = escape_graphql_string(session_id);
        let sample = gents::graphql::graphql_with_transaction_retry(
            node,
            &format!(
                r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 1) {{ _docID agent_did requester_did }} }}"#
            ),
            "sample",
        )
        .await?
        .data
        .ok_or_else(|| anyhow::anyhow!("no data"))?;
        let row = &rows(&sample, "AgentOutputSegment")[0];
        let (id, agent, requester) = (
            row["_docID"].as_str().unwrap().to_string(),
            row["agent_did"].as_str().unwrap().to_string(),
            row["requester_did"].as_str().unwrap().to_string(),
        );
        let scope = format!(
            r#"agent_did: {{ _eq: "{}" }}, requester_did: {{ _eq: "{}" }}"#,
            escape_graphql_string(&agent),
            escape_graphql_string(&requester)
        );
        eprintln!("--- {label}");
        timed("  segment by _docID, scoped", 7, || {
            let q = format!(
                r#"{{ AgentOutputSegment(filter: {{ _docID: {{ _eq: "{id}" }}, {scope} }}, limit: 2) {{ _docID ordinal }} }}"#
            );
            async move {
                gents::graphql::graphql_with_transaction_retry(node, &q, "bench")
                    .await?
                    .data
                    .ok_or_else(|| anyhow::anyhow!("no data"))
            }
        })
        .await?;
        timed("  segments by session_id, 40", 7, || {
            let q = format!(
                r#"{{ AgentOutputSegment(filter: {{ session_id: {{ _eq: "{session}" }} }}, limit: 40) {{ _docID ordinal }} }}"#
            );
            async move {
                gents::graphql::graphql_with_transaction_retry(node, &q, "bench")
                    .await?
                    .data
                    .ok_or_else(|| anyhow::anyhow!("no data"))
            }
        })
        .await?;
        timed("  transcript page tip (40)", 5, || {
            let (agent, requester) = (&agent, &requester);
            async move {
                load_session_transcript_page(
                    node,
                    session_id,
                    Some(agent),
                    Some(requester),
                    None,
                    Some(40),
                )
                .await
            }
        })
        .await?;
        Ok(())
    }

    let core =
        ClientCore::start_with_paths_and_options(paths(), ClientCoreOptions::local_only()).await?;
    probe(&core, &session_id, "baseline (fresh open)").await?;

    // Writes shaped like a running agent: repeated updates of one request
    // document, plus new session documents.
    let request = gents::graphql::graphql_with_transaction_retry(
        core.node(),
        &format!(
            r#"{{ AgentRequest(filter: {{ session_id: {{ _eq: "{}" }} }}, limit: 1) {{ _docID lifecycle_state }} }}"#,
            escape_graphql_string(&session_id)
        ),
        "request",
    )
    .await?
    .data
    .ok_or_else(|| anyhow::anyhow!("no data"))?;
    let request_id = rows(&request, "AgentRequest")[0]["_docID"]
        .as_str()
        .unwrap()
        .to_string();
    let started = Instant::now();
    let mut failures = 0;
    for i in 0..writes {
        let state = if i % 2 == 0 { "completed" } else { "failed" };
        let update = core
            .node()
            .execute(&format!(
                r#"mutation {{ update_AgentRequest(filter: {{ _docID: {{ _eq: "{request_id}" }} }}, input: {{ lifecycle_state: "{state}" }}) {{ _docID }} }}"#
            ))
            .await;
        if update.has_errors() {
            failures += 1;
            if failures == 1 {
                eprintln!("  update error: {:?}", update.errors);
            }
        }
        let create = core
            .node()
            .execute(&format!(
                r#"mutation {{ create_AgentSession(input: {{ session_id: "bench-{i}", agent_did: "did:key:bench", behavior_id: "default", created_at: "2026-09-30T00:00:00Z" }}) {{ _docID }} }}"#
            ))
            .await;
        if create.has_errors() {
            failures += 1;
            if failures == 1 {
                eprintln!("  create error: {:?}", create.errors);
            }
        }
    }
    eprintln!(
        "\nwrote {writes} updates + {writes} creates in {:.0} ms ({failures} failed)\n",
        as_ms(started.elapsed())
    );
    probe(&core, &session_id, "after in-process writes").await?;

    core.shutdown().await?;
    drop(core);
    let core =
        ClientCore::start_with_paths_and_options(paths(), ClientCoreOptions::local_only()).await?;
    probe(&core, &session_id, "after reopen (WAL replay)").await?;
    Ok(())
}
