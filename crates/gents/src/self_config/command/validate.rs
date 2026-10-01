use super::*;
use crate::config_client::{
    validate_desired_state_plan, ConfigAccess, DesiredStateApplyDocument, DesiredStateApplyPlan,
};

impl ConfigCommandTool {
    pub(super) async fn validate_saved(&self, argv: &[String]) -> Result<String> {
        self.ensure_behavior_catalog("validate", None)?;
        anyhow::ensure!(argv.is_empty(), "validate takes no parameters; it checks saved configuration for your authenticated principal");
        let identity = self.core.identity()?;
        let (counts, errors) = ConfigAccess::transact_local(
            &self.node,
            Some(identity),
            "self_config.validate",
            |txn| Box::pin(async move {
                let references = crate::ConfigReferences::load_in_txn(txn, &self.agent_did).await?;
                let mut counts = BTreeMap::<&str, usize>::new();
                let mut errors = Vec::new();
                for ((collection, id), document) in references.documents() {
                    *counts.entry(collection.graphql_type()).or_default() += 1;
                    if let Err(error) = references.validate_document(*collection, document) {
                        let mut diagnostic = json!({
                            "collection": collection.graphql_type(),
                            "id": id,
                            "error": format!("{error:#}")
                        });
                        if let Some(missing) = error.downcast_ref::<crate::document_config::MissingReference>() {
                            diagnostic["field"] = json!(missing.field);
                            diagnostic["missing"] = json!({"collection":missing.target.graphql_type(),"id":missing.target_id});
                        }
                        if let Some((resource, _)) = HELP_INDEX.iter().find(|(resource, _)| {
                            crud::resource_target(resource).is_some_and(|target| target.collection_name() == collection.graphql_type())
                                && model_resources(&self.categories, self.allow_pack_install).contains(resource)
                        }) {
                            diagnostic["inspect_with"] = json!({"argv":[resource,"get"],"target_id":id});
                        }
                        errors.push(diagnostic);
                    }
                }
                if errors.is_empty() {
                    // Revalidate the saved snapshot without publishing it. Marking every
                    // document as a candidate also runs the owner's affected-field checks.
                    let plan = DesiredStateApplyPlan::new(references.documents().map(|((collection, _), value)| DesiredStateApplyDocument {
                        collection: *collection, add: value.clone(), update: value.clone(),
                    }).collect())?;
                    if let Err(error) = validate_desired_state_plan(txn, &plan).await {
                        if crate::config_client::is_transaction_step_unavailable(&error) {
                            return Err(error);
                        }
                        errors.push(json!({"error":format!("{error:#}")}));
                    }
                }
                Ok((counts, errors))
            }),
        ).await?;
        ordered! {
            "valid": errors.is_empty(),
            "checked_documents": counts.values().sum::<usize>(),
            "collections": counts,
            "errors": errors,
            "scope": "Saved configuration for the authenticated principal: canonical fields, references and publication checks. Reports the first failure per document, then publication checks; fix and rerun. Does not test credentials, remote destinations, runtime or application-schema readiness, or user intent.",
            "next": if errors.is_empty() { "Inspect behavior get to verify selections match the user's request; exercise tools to verify runtime behavior. Report what remains untested." } else { "Inspect the named objects, fix them with resource update/create, and run validate again before reporting completion." },
            "committed": false,
        }.pretty()
    }
}
