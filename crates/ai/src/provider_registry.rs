use crate::LLMId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDef {
    pub id: &'static str,
    pub label: &'static str,
    pub auth_type: AuthType,
    pub key_placeholder: &'static str,
    pub key_env_var: &'static str,
    pub docs_url: &'static str,
    pub models: &'static [ModelDef],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDef {
    pub id: &'static str,
    pub label: &'static str,
    pub context_window: u32,
    pub supports_tools: bool,
}

impl ModelDef {
    pub fn llm_id(&self) -> LLMId {
        LLMId::from(self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    ApiKey,
    OAuth { authorize_url: &'static str },
    AwsBedrock,
}

pub const ANTHROPIC_MODELS: &[ModelDef] = &[
    ModelDef {
        id: "claude-opus-4",
        label: "Claude Opus 4",
        context_window: 200_000,
        supports_tools: true,
    },
    ModelDef {
        id: "claude-sonnet-4",
        label: "Claude Sonnet 4",
        context_window: 200_000,
        supports_tools: true,
    },
    ModelDef {
        id: "claude-3-5-haiku",
        label: "Claude 3.5 Haiku",
        context_window: 200_000,
        supports_tools: true,
    },
];

pub const OPENAI_MODELS: &[ModelDef] = &[
    ModelDef {
        id: "gpt-4o",
        label: "GPT-4o",
        context_window: 128_000,
        supports_tools: true,
    },
    ModelDef {
        id: "o3",
        label: "o3",
        context_window: 200_000,
        supports_tools: true,
    },
    ModelDef {
        id: "o4-mini",
        label: "o4-mini",
        context_window: 200_000,
        supports_tools: true,
    },
];

pub const GOOGLE_MODELS: &[ModelDef] = &[
    ModelDef {
        id: "gemini-2.5-pro",
        label: "Gemini 2.5 Pro",
        context_window: 1_000_000,
        supports_tools: true,
    },
    ModelDef {
        id: "gemini-2.0-flash",
        label: "Gemini 2.0 Flash",
        context_window: 1_000_000,
        supports_tools: true,
    },
];

pub const OPEN_ROUTER_MODELS: &[ModelDef] = &[ModelDef {
    id: "openrouter/custom",
    label: "OpenRouter custom model",
    context_window: 0,
    supports_tools: true,
}];

pub const AWS_BEDROCK_MODELS: &[ModelDef] = &[
    ModelDef {
        id: "aws-bedrock/claude-sonnet-4",
        label: "Claude Sonnet 4 on Bedrock",
        context_window: 200_000,
        supports_tools: true,
    },
    ModelDef {
        id: "aws-bedrock/claude-3-5-haiku",
        label: "Claude 3.5 Haiku on Bedrock",
        context_window: 200_000,
        supports_tools: true,
    },
];

pub const CUSTOM_MODELS: &[ModelDef] = &[ModelDef {
    id: "custom/openai-compatible",
    label: "Custom OpenAI-compatible model",
    context_window: 0,
    supports_tools: true,
}];

pub const PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        id: "anthropic",
        label: "Anthropic",
        auth_type: AuthType::ApiKey,
        key_placeholder: "sk-ant-...",
        key_env_var: "ANTHROPIC_API_KEY",
        docs_url: "https://docs.anthropic.com/en/api/admin-api/apikeys/get-api-key",
        models: ANTHROPIC_MODELS,
    },
    ProviderDef {
        id: "openai",
        label: "OpenAI",
        auth_type: AuthType::ApiKey,
        key_placeholder: "sk-...",
        key_env_var: "OPENAI_API_KEY",
        docs_url: "https://platform.openai.com/api-keys",
        models: OPENAI_MODELS,
    },
    ProviderDef {
        id: "google",
        label: "Google",
        auth_type: AuthType::ApiKey,
        key_placeholder: "AIzaSy...",
        key_env_var: "GEMINI_API_KEY",
        docs_url: "https://aistudio.google.com/app/apikey",
        models: GOOGLE_MODELS,
    },
    ProviderDef {
        id: "open_router",
        label: "OpenRouter",
        auth_type: AuthType::ApiKey,
        key_placeholder: "sk-or-...",
        key_env_var: "OPENROUTER_API_KEY",
        docs_url: "https://openrouter.ai/settings/keys",
        models: OPEN_ROUTER_MODELS,
    },
    ProviderDef {
        id: "aws_bedrock",
        label: "AWS Bedrock",
        auth_type: AuthType::AwsBedrock,
        key_placeholder: "",
        key_env_var: "AWS_PROFILE",
        docs_url: "https://docs.aws.amazon.com/bedrock/latest/userguide/getting-started.html",
        models: AWS_BEDROCK_MODELS,
    },
    ProviderDef {
        id: "custom",
        label: "Custom OpenAI-compatible endpoint",
        auth_type: AuthType::ApiKey,
        key_placeholder: "sk-...",
        key_env_var: "OPENAI_COMPATIBLE_API_KEY",
        docs_url: "https://platform.openai.com/docs/api-reference",
        models: CUSTOM_MODELS,
    },
];

pub fn providers() -> &'static [ProviderDef] {
    PROVIDERS
}

pub fn provider_by_id(id: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}
