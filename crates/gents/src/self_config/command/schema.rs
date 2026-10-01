use super::*;

fn schema_recovery(error: anyhow::Error) -> anyhow::Error {
    let Some(mismatch) = error.downcast_ref::<crate::config_client::SchemaInstallMismatch>() else {
        return error;
    };
    CommandGuidance {
        message: format!("{error:#}. Existing schemas cannot be replaced through schema install. Inspect the saved schema; preserve the user's collection names and report the limitation if the requested contract cannot be met."),
        next_call: json!({"argv":["schema","get"],"target_id":mismatch.collection}),
    }.into()
}

impl ConfigCommandTool {
    pub(super) async fn schema(&self, argv: &[String]) -> Result<String> {
        self.ensure_resource("automation")?;
        self.core.identity()?;
        let access = crate::config_client::ConfigAccess::Local(self.node.clone());
        if argv.first().is_some_and(|arg| arg == "get") {
            anyhow::ensure!(argv.len() == 2, "schema get requires COLLECTION");
            return Ok(serde_json::to_string_pretty(
                &access
                    .collection_version(&argv[1])
                    .await?
                    .context("collection is not registered")?,
            )?);
        }
        let preview = argv.first().is_some_and(|arg| arg == "preview");
        let argv = if preview { &argv[1..] } else { argv };
        if !argv.first().is_some_and(|arg| arg == "install") {
            return Err(CommandGuidance {
                message: "schema supports get COLLECTION and preview install/install; it does not support document CRUD or list.".into(),
                next_call: json!({"argv":["schema","--help"]}),
            }.into());
        }
        let parsed = ParsedArgs::parse(&argv[1..])?;
        anyhow::ensure!(
            parsed.positionals.is_empty() && parsed.switches.is_empty(),
            "schema install accepts --sdl and --digest only"
        );
        for name in parsed.options.keys() {
            anyhow::ensure!(
                matches!(name.as_str(), "sdl" | "digest"),
                "unknown schema option --{name}"
            );
        }
        let sdl = parsed.one("sdl")?.context("--sdl SDL is required")?;
        anyhow::ensure!(
            sdl.len() <= 64 * 1024,
            "schema SDL exceeds 64 KiB; submit a smaller schema"
        );
        let digest = parsed.one("digest")?;
        let plan = if preview {
            anyhow::ensure!(self.preview, "schema preview is not granted");
            anyhow::ensure!(
                digest.is_none(),
                "preview returns the digest; do not supply --digest"
            );
            crate::config_client::preview_schema_install(&access, sdl)
                .await
                .map_err(schema_recovery)?
        } else {
            let digest = digest.context("--digest from schema preview install is required")?;
            self.execution.enter_mutation();
            crate::config_client::apply_schema_install(&access, sdl, digest)
                .await
                .map_err(schema_recovery)?
        };
        ordered! {
            "committed": !preview && plan.requires_publication,
            "verified": !preview,
            "before_install": if preview && plan.requires_publication {
                Some("Existing collection shapes cannot be changed here. If you choose Task.emit_outcome for standard run records, declare handoff_id: String on its source collection and populate it in write tools; see [\"help\",\"datastore\"]. Keep caller business keys separate.")
            } else { None },
            "scope": "Schema contracts are node-wide. Document reads/writes still require DefraDB ACP and explicit datastore tool selection. Schema publication is separate from configuration document transactions.",
            "plan": plan,
        }
        .pretty()
    }
}
