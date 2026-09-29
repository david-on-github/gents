//! Installing a pack's plugins into the home's plugin store.
//!
//! Moved here from `gents-cli` (D5's home-for-`self_config`, so a graph
//! install driven by `self_config` can install a pack's plugins itself,
//! rather than asking the CLI to do it). The CLI's own `commands::pack` and
//! `commands::plugin` modules keep thin callers that forward here.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::authority::describe_plugin_authority;
use super::store::{self, InstalledPlugin};
use super::{Manifold, PluginRunner};
use crate::pack::{PackManifest, PackPlugin};

/// Installs one plugin a pack carries into the same content-addressed
/// store `gents plugin install` uses, so a plugin that arrived inside a
/// pack runs by name (`gents plugin run <name>`) just like one installed on
/// its own.
///
/// A plugin declared inline in a pack manifest carries neither a namespace
/// nor a version of its own, so it takes its pack's: two packs from
/// different namespaces may each carry a `format_check`, and recording both
/// under one default namespace would have the second silently replace the
/// first.
///
/// `pack_coordinate` (`{namespace}/{name}` of the pack this plugin ships in)
/// is refused when the record already belongs to a different pack: see
/// [`store::check_plugin_ownership`].
#[allow(clippy::too_many_arguments)]
pub fn install_from_pack(
    home: &Path,
    pack_namespace: &str,
    pack_coordinate: &str,
    pack_version: &str,
    pack_digest: &str,
    plugin: &PackPlugin,
    artifact_bytes: &[u8],
    instructions: Option<String>,
    consent: bool,
) -> Result<InstalledPlugin> {
    store::check_plugin_ownership(home, pack_namespace, &plugin.name, pack_coordinate)?;
    let granted = store::grant_on_install(home, pack_namespace, plugin, consent)?;
    PluginRunner::compile(artifact_bytes, plugin)
        .with_context(|| format!("admitting pack plugin {}", plugin.name))?;
    let effective = granted.clone().unwrap_or_else(Manifold::sealed);
    if let Some(description) = describe_plugin_authority(plugin, &effective) {
        tracing::info!(
            plugin = %plugin.name,
            namespace = pack_namespace,
            authority = %description,
            "installed pack plugin",
        );
    }
    let digest_hex = format!("{:x}", Sha256::digest(artifact_bytes));
    // Shared against `release_unreferenced_bytes`'s exclusive lock: bytes and
    // the record that points at them are written before a concurrent
    // `gents pack remove` can decide those bytes are unreferenced (store.rs's
    // own doc).
    let _lock = store::lock_store(home, false)?;
    store::store_bytes(home, &digest_hex, artifact_bytes)?;
    let record = InstalledPlugin {
        namespace: pack_namespace.to_owned(),
        name: plugin.name.clone(),
        version: pack_version.to_owned(),
        digest: format!("sha256:{digest_hex}"),
        language: plugin.language.clone(),
        declaration: plugin.clone(),
        granted,
        instructions,
        owner_pack_coordinate: Some(pack_coordinate.to_owned()),
        owner_pack_digest: Some(pack_digest.to_owned()),
    };
    store::write_record(home, &record)?;
    Ok(record)
}

/// Admits and stores every plugin a pack ships in `home`'s plugin store.
/// `pack_digest` is the pack's own content digest, recorded on each
/// plugin's record for operator visibility (see
/// [`InstalledPlugin::owner_pack_digest`]).
pub fn install_pack_plugins<'a>(
    home: &Path,
    manifest: &PackManifest,
    pack_digest: &str,
    asset: impl Fn(&str) -> Result<&'a [u8]>,
    consent: bool,
) -> Result<Vec<InstalledPlugin>> {
    let pack_coordinate = format!("{}/{}", manifest.metadata.namespace, manifest.name);
    manifest
        .metadata
        .plugins
        .iter()
        .map(|plugin| {
            let instructions = plugin
                .instructions
                .as_deref()
                .map(|path| crate::pack::tool_instructions(&plugin.name, asset(path)?))
                .transpose()?;
            install_from_pack(
                home,
                &manifest.metadata.namespace,
                &pack_coordinate,
                &manifest.version,
                pack_digest,
                plugin,
                asset(&plugin.artifact)?,
                instructions,
                consent,
            )
        })
        .collect()
}

/// Every plugin `manifest` would install and its plugin-store record before
/// any write, so a failure later in the same pack install can restore
/// exactly what was there (or remove what was not) instead of leaving an
/// orphaned plugin record behind. `None` means the name was not installed.
pub fn snapshot_pack_plugin_records(
    home: &Path,
    manifest: &PackManifest,
) -> Vec<(String, String, Option<InstalledPlugin>)> {
    manifest
        .metadata
        .plugins
        .iter()
        .map(|plugin| {
            let previous =
                store::read_record(home, &manifest.metadata.namespace, &plugin.name).ok();
            (
                manifest.metadata.namespace.clone(),
                plugin.name.clone(),
                previous,
            )
        })
        .collect()
}

/// Restores each `(namespace, name)` plugin record to what
/// [`snapshot_pack_plugin_records`] observed before the install that must
/// now be undone: the previous record is put back, or removed if there was
/// none. Best-effort and never fails the caller: a restore that cannot
/// complete is logged loudly rather than masking the original error that
/// triggered the rollback.
pub fn rollback_pack_plugin_records(
    home: &Path,
    previous: &[(String, String, Option<InstalledPlugin>)],
) {
    for (namespace, name, record) in previous {
        let result = match record {
            Some(record) => store::write_record(home, record),
            None => match store::read_record(home, namespace, name) {
                Ok(_) => store::remove_record(home, namespace, name).map(|_| ()),
                // Never written by this install (it failed before reaching
                // this plugin, or this plugin failed itself): nothing to undo.
                Err(_) => Ok(()),
            },
        };
        if let Err(error) = result {
            tracing::error!(
                namespace,
                name,
                error = %error,
                "failed to roll back a plugin record after a failed pack install",
            );
        }
    }
}
