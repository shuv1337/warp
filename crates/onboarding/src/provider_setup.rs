use std::collections::{HashMap, HashSet};

use ai::api_keys::CustomEndpointConfig;
use ai::provider_registry::{provider_by_id, providers, AuthType, ModelDef, ProviderDef};
use ai::LLMId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderSetupStep {
    SelectProviders,
    CollectCredentials {
        provider_idx: usize,
        inputs: HashMap<String, String>,
    },
    SelectDefaultModel,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderCredentialUpdate {
    Anthropic(Option<String>),
    OpenAI(Option<String>),
    Google(Option<String>),
    OpenRouter(Option<String>),
    CustomEndpoint(Option<CustomEndpointConfig>),
    AwsBedrock,
}

#[derive(Clone, Debug)]
pub struct ProviderSetupState {
    step: ProviderSetupStep,
    selected_provider_ids: Vec<&'static str>,
    authenticated_provider_ids: HashSet<&'static str>,
    selected_default_model: Option<LLMId>,
}

impl Default for ProviderSetupState {
    fn default() -> Self {
        Self {
            step: ProviderSetupStep::SelectProviders,
            selected_provider_ids: Vec::new(),
            authenticated_provider_ids: HashSet::new(),
            selected_default_model: None,
        }
    }
}

impl ProviderSetupState {
    pub fn step(&self) -> &ProviderSetupStep {
        &self.step
    }

    pub fn available_providers(&self) -> &'static [ProviderDef] {
        providers()
    }

    pub fn selected_provider_ids(&self) -> &[&'static str] {
        &self.selected_provider_ids
    }

    pub fn selected_default_model(&self) -> Option<&LLMId> {
        self.selected_default_model.as_ref()
    }

    pub fn set_provider_selected(&mut self, provider_id: &'static str, selected: bool) {
        if provider_by_id(provider_id).is_none() {
            return;
        }

        if selected {
            if !self.selected_provider_ids.contains(&provider_id) {
                self.selected_provider_ids.push(provider_id);
            }
        } else {
            self.selected_provider_ids.retain(|id| *id != provider_id);
            self.authenticated_provider_ids.remove(provider_id);
        }
    }

    pub fn begin_credentials(&mut self) {
        if self.selected_provider_ids.is_empty() {
            return;
        }
        self.step = ProviderSetupStep::CollectCredentials {
            provider_idx: 0,
            inputs: HashMap::new(),
        };
    }

    pub fn current_provider(&self) -> Option<&'static ProviderDef> {
        let ProviderSetupStep::CollectCredentials { provider_idx, .. } = self.step else {
            return None;
        };
        self.selected_provider_ids
            .get(provider_idx)
            .and_then(|id| provider_by_id(id))
    }

    pub fn credential_update_for_current_provider(
        &self,
        inputs: &HashMap<String, String>,
    ) -> Option<ProviderCredentialUpdate> {
        let provider = self.current_provider()?;
        match provider.id {
            "anthropic" => Some(ProviderCredentialUpdate::Anthropic(api_key_input(inputs))),
            "openai" => Some(ProviderCredentialUpdate::OpenAI(api_key_input(inputs))),
            "google" => Some(ProviderCredentialUpdate::Google(api_key_input(inputs))),
            "open_router" => Some(ProviderCredentialUpdate::OpenRouter(api_key_input(inputs))),
            "custom" => {
                let base_url = inputs.get("base_url")?.trim();
                if base_url.is_empty() {
                    return None;
                }
                Some(ProviderCredentialUpdate::CustomEndpoint(Some(
                    CustomEndpointConfig {
                        base_url: base_url.to_owned(),
                        api_key: api_key_input(inputs),
                        model_prefix: optional_input(inputs, "model_prefix"),
                    },
                )))
            }
            "aws_bedrock" => Some(ProviderCredentialUpdate::AwsBedrock),
            _ => None,
        }
    }

    pub fn complete_current_provider(&mut self) {
        let ProviderSetupStep::CollectCredentials { provider_idx, .. } = self.step else {
            return;
        };
        if let Some(provider_id) = self.selected_provider_ids.get(provider_idx) {
            self.authenticated_provider_ids.insert(*provider_id);
        }

        let next_idx = provider_idx + 1;
        if next_idx < self.selected_provider_ids.len() {
            self.step = ProviderSetupStep::CollectCredentials {
                provider_idx: next_idx,
                inputs: HashMap::new(),
            };
        } else {
            self.step = ProviderSetupStep::SelectDefaultModel;
        }
    }

    pub fn authenticated_models(&self) -> Vec<&'static ModelDef> {
        self.authenticated_provider_ids
            .iter()
            .filter_map(|id| provider_by_id(id))
            .flat_map(|provider| provider.models.iter())
            .collect()
    }

    pub fn select_default_model(&mut self, model_id: LLMId) -> bool {
        if self
            .authenticated_models()
            .iter()
            .any(|model| model.id == model_id.as_str())
        {
            self.selected_default_model = Some(model_id);
            self.step = ProviderSetupStep::Done;
            true
        } else {
            false
        }
    }

    pub fn auth_badge(provider: &ProviderDef) -> &'static str {
        match provider.auth_type {
            AuthType::ApiKey => "API key",
            AuthType::OAuth { .. } => "OAuth",
            AuthType::AwsBedrock => "AWS credentials",
        }
    }
}

fn api_key_input(inputs: &HashMap<String, String>) -> Option<String> {
    optional_input(inputs, "api_key")
}

fn optional_input(inputs: &HashMap<String, String>, key: &str) -> Option<String> {
    inputs
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
