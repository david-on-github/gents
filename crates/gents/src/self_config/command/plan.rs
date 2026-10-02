use super::*;
use crate::config_client::{
    read_desired_state_record_in_txn, validate_desired_state_plan, ConfigAccess,
    DesiredStateApplyDocument, DesiredStateApplyPlan,
};
use crate::Collection;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedDocument {
    collection: String,
    document: Value,
    #[serde(default)]
    mailbox: Option<crate::mailbox::MailboxNotificationPolicy>,
}

/// The grant category a connected plan proposal of `collection` needs, or
/// `None` when plan preview does not stage that collection.
pub(super) fn plan_category(collection: Collection) -> Option<&'static str> {
    Some(match collection {
        Collection::AgentBehavior | Collection::AgentContext => "persona",
        Collection::Tools | Collection::DatastoreToolSurface | Collection::SubagentTarget => {
            "tools"
        }
        Collection::InferenceExecution => "profile",
        Collection::Task | Collection::Schedule | Collection::Trigger | Collection::EventSource => {
            "automation"
        }
        _ => return None,
    })
}

impl ConfigCommandTool {
    /// Names what an unknown plan collection is: a misspelled canonical name,
    /// an installed application collection, or neither.
    async fn unknown_plan_collection(&self, name: &str) -> String {
        let supported = Collection::ALL
            .iter()
            .filter(|collection| plan_category(**collection).is_some())
            .map(|collection| collection.graphql_type())
            .collect::<Vec<_>>()
            .join(", ");
        let canonical = Collection::ALL
            .iter()
            .map(|collection| collection.graphql_type().to_owned())
            .collect::<Vec<_>>();
        let installed = ConfigAccess::Local(self.node.clone())
            .collection_version(name)
            .await
            .ok()
            .flatten()
            .is_some();
        if installed {
            return format!(
                "{name:?} is an application collection, not configuration; plan preview stages only {supported}. Its documents are written by datastore tools: [\"help\",\"datastore\"]"
            );
        }
        let fold = |name: &str| name.replace(['_', '-', ' '], "").to_ascii_lowercase();
        let near = canonical
            .iter()
            .find(|candidate| fold(candidate) == fold(name))
            .or_else(|| {
                canonical
                    .iter()
                    .find(|candidate| fold(candidate).ends_with(&fold(name)))
            })
            .cloned()
            .or_else(|| {
                crate::defra_query::schema::suggest_fields(name, &canonical)
                    .into_iter()
                    .next()
            });
        match near {
            Some(near) => format!(
                "unknown collection {name:?}; did you mean {near:?}? Plan collections: {supported}"
            ),
            None => format!(
                "{name:?} is neither a configuration collection nor an installed schema; plan preview stages only {supported}. An application collection needs its schema installed first ([\"help\",\"schema\"]) and its documents are written by datastore tools, not plan preview"
            ),
        }
    }

    pub(super) async fn plan(&self, argv: &[String]) -> Result<String> {
        anyhow::ensure!(self.preview, "preview is not granted for this behavior");
        self.ensure_behavior_catalog("plan", None)?;
        anyhow::ensure!(
            argv.first().is_some_and(|word| word == "preview"),
            "plan supports preview only; see [\"help\",\"plan\"]"
        );
        let parsed = ParsedArgs::parse(&argv[1..])?;
        anyhow::ensure!(
            parsed.positionals.is_empty()
                && parsed.switches.is_empty()
                && parsed.options.keys().all(|key| key == "documents"),
            "plan preview accepts only --documents"
        );
        let encoded = parsed
            .one("documents")?
            .context("--documents is required")?;
        anyhow::ensure!(encoded.len() <= 128 * 1024, "plan exceeds 128 KiB");
        let proposed: Vec<ProposedDocument> = serde_json::from_str(encoded)?;
        anyhow::ensure!(
            !proposed.is_empty() && proposed.len() <= 64,
            "plan requires 1 to 64 documents"
        );
        let mut documents = Vec::new();
        for mut item in proposed {
            let Some(collection) = Collection::ALL
                .iter()
                .copied()
                .find(|collection| collection.graphql_type() == item.collection)
            else {
                bail!(self.unknown_plan_collection(&item.collection).await);
            };
            let Some(category) = plan_category(collection) else {
                bail!(
                    "{} is not a supported plan resource; use its existing resource commands",
                    item.collection
                );
            };
            self.ensure_resource(category)?;
            if let Some(policy) = item.mailbox {
                anyhow::ensure!(
                    collection == Collection::DatastoreToolSurface,
                    "mailbox policy is only supported on DatastoreToolSurface proposals"
                );
                let document = item
                    .document
                    .as_object_mut()
                    .context("proposed document must be an object")?;
                anyhow::ensure!(
                    !document.contains_key("entries"),
                    "mailbox policy cannot be combined with explicit entries"
                );
                document.insert("entries".into(), super::datastore::mailbox_entries(policy)?);
            }
            anyhow::ensure!(
                item.document["agent_did"].as_str() == Some(self.agent_did.as_str()),
                "proposed document must belong to this principal"
            );
            documents.push(DesiredStateApplyDocument {
                collection,
                add: item.document.clone(),
                update: item.document,
            });
        }
        let plan = DesiredStateApplyPlan::new(documents)?;
        let identity = self.core.identity()?;
        ConfigAccess::transact_local(
            &self.node,
            Some(identity),
            "self_config.plan.preview",
            |txn| {
                let plan = &plan;
                Box::pin(async move {
                    for document in plan.documents() {
                        let id = document.add[document.collection.unique_field()]
                            .as_str()
                            .context("document ID missing")?;
                        anyhow::ensure!(
                            read_desired_state_record_in_txn(
                                txn,
                                document.collection,
                                &self.agent_did,
                                id
                            )
                            .await?
                            .is_none(),
                            "{} {id:?} already exists; plan preview accepts new documents only",
                            document.collection.graphql_type()
                        );
                    }
                    validate_desired_state_plan(txn, plan).await
                })
            },
        )
        .await?;
        ordered! {
            "committed": false,
            "validation_scope": "canonical document shapes and retained references; schema publication, approval and live readiness remain separate",
            "documents": plan.documents().iter().map(|document| json!({"collection":document.collection.graphql_type(),"document":document.add})).collect::<Vec<_>>(),
        }
        .pretty()
    }
}
