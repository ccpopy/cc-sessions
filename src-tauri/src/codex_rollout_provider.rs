use serde_json::Value;
use std::collections::HashMap;

/// session_meta describes creation. Native resume persists subsequent provider choices in
/// thread_settings_applied; copied snapshots still belong to their original logical thread.
#[derive(Default)]
pub(crate) struct EffectiveProviderTracker {
    header_id: Option<String>,
    providers: HashMap<String, String>,
}

impl EffectiveProviderTracker {
    pub(crate) fn observe(&mut self, value: &Value) {
        let payload = &value["payload"];
        if value["type"] == "session_meta" {
            if let Some(id) = payload["id"].as_str() {
                self.header_id.get_or_insert_with(|| id.to_owned());
                if let Some(provider) = payload["model_provider"].as_str() {
                    self.providers
                        .entry(id.to_owned())
                        .or_insert_with(|| provider.to_owned());
                }
            }
        } else if value["type"] == "event_msg" && payload["type"] == "thread_settings_applied" {
            let owner = payload["thread_id"].as_str().or(self.header_id.as_deref());
            if let (Some(owner), Some(provider)) = (
                owner,
                payload["thread_settings"]["model_provider_id"].as_str(),
            ) {
                if !provider.is_empty() {
                    self.providers.insert(owner.to_owned(), provider.to_owned());
                }
            }
        }
    }

    pub(crate) fn effective_for(&self, id: &str) -> Option<String> {
        self.providers.get(id).cloned()
    }
}
