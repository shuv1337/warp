use std::{collections::HashMap, sync::Arc};

use crate::{ai::agent::redaction, terminal::model::session::SessionType};
use futures_util::StreamExt;
use warp_core::features::FeatureFlag;
use warp_multi_agent_api as api;

use crate::server::server_api::ServerApi;

use super::{convert_to::convert_input, ConvertToAPITypeError, RequestParams, ResponseStream};

pub async fn generate_multi_agent_output(
    server_api: Arc<ServerApi>,
    mut params: RequestParams,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> Result<ResponseStream, ConvertToAPITypeError> {
    let supported_tools = params
        .supported_tools_override
        .take()
        .unwrap_or_else(|| get_supported_tools(&params));
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);
    let mut logging_metadata = HashMap::new();
    if let Some(metadata) = params.metadata {
        logging_metadata.insert(
            "is_autodetected_user_query".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_autodetected_user_query,
                )),
            },
        );
        logging_metadata.insert(
            "entrypoint".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    metadata.entrypoint.entrypoint(),
                )),
            },
        );
        logging_metadata.insert(
            "is_auto_resume_after_error".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_auto_resume_after_error,
                )),
            },
        );
    }

    if params.should_redact_secrets {
        redaction::redact_inputs(&mut params.input);
    }

    let mut api_keys = params.api_keys;
    if let Some(api_keys) = &mut api_keys {
        api_keys.allow_use_of_warp_credits = params.allow_use_of_warp_credits_with_byok;
    }

    let request = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: params.tasks,
        }),
        input: Some(convert_input(params.input)?),
        settings: Some(api::request::Settings {
            model_config: Some(api::request::settings::ModelConfig {
                base: params.model.into(),
                cli_agent: params.cli_agent_model.into(),
                computer_use_agent: params.computer_use_model.into(),
                ..Default::default()
            }),
            rules_enabled: params.is_memory_enabled,
            warp_drive_context_enabled: params.warp_drive_context_enabled,
            web_context_retrieval_enabled: true,
            supports_parallel_tool_calls: true,
            use_anthropic_text_editor_tools: false,
            planning_enabled: params.planning_enabled,
            supports_create_files: true,
            supported_tools: supported_tools.into_iter().map(Into::into).collect(),
            supports_long_running_commands: true,
            should_preserve_file_content_in_history: true,
            supports_todos_ui: true,
            supports_linked_code_blocks: FeatureFlag::LinkedCodeBlocks.is_enabled(),
            supports_started_child_task_message: true,
            supports_suggest_prompt: true,
            supports_read_image_files: FeatureFlag::ReadImageFiles.is_enabled(),
            supports_reasoning_message: true,
            api_keys,
            autonomy_level: params.autonomy_level.into(),
            isolation_level: params.isolation_level.into(),
            web_search_enabled: params.web_search_enabled,
            supported_cli_agent_tools: supported_cli_agent_tools
                .into_iter()
                .map(Into::into)
                .collect(),
            supports_v4a_file_diffs: FeatureFlag::V4AFileDiffs.is_enabled(),
            supports_summarization_via_message_replacement:
                FeatureFlag::SummarizationViaMessageReplacement.is_enabled(),
            supports_bundled_skills: FeatureFlag::BundledSkills.is_enabled(),
            supports_research_agent: params.research_agent_enabled,
            supports_orchestration_v2: FeatureFlag::OrchestrationV2.is_enabled(),
        }),
        metadata: Some(api::request::Metadata {
            logging: logging_metadata,
            conversation_id: params
                .conversation_token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            ambient_agent_task_id: params
                .ambient_agent_task_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            forked_from_conversation_id: if params.conversation_token.is_none() {
                // We only include this param on our initial request to the server
                // (when the forked conversation has not been asigned a new id yet).
                params
                    .forked_from_conversation_token
                    .map(|token| token.as_str().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            },
            parent_agent_id: params.parent_agent_id.unwrap_or_default(),
            agent_name: params.agent_name.unwrap_or_default(),
        }),
        existing_suggestions: params
            .existing_suggestions
            .map(|suggestions| suggestions.into()),
        mcp_context: params.mcp_context.map(Into::into),
    };

    log_byok_request_shape(&request);

    let response_stream = server_api.generate_multi_agent_output(&request).await;
    match response_stream {
        Ok(stream) => {
            let output_stream = stream.take_until(cancellation_rx);
            Ok(Box::pin(output_stream))
        }
        Err(e) => {
            let (tx, rx) = async_channel::unbounded();
            let _ = tx.send(Err(e)).await;
            Ok(Box::pin(rx))
        }
    }
}

/// Sanitized debug log of the outgoing multi-agent request shape.
///
/// Logs only structural metadata (model IDs, which API key fields are populated,
/// task counts, conversation IDs, input variant). Never logs key values, prompt
/// text, or tool arguments. Intended for diagnosing BYOK 400s from the Warp
/// `/ai/multi-agent` proxy.
fn log_byok_request_shape(request: &api::Request) {
    let settings = request.settings.as_ref();
    let model_config = settings.and_then(|s| s.model_config.as_ref());
    let api_keys = settings.and_then(|s| s.api_keys.as_ref());
    let task_context = request.task_context.as_ref();
    let metadata = request.metadata.as_ref();

    let model_base = model_config.map(|m| m.base.as_str()).unwrap_or("<none>");
    let model_coding = model_config.map(|m| m.coding.as_str()).unwrap_or("<none>");
    let model_cli_agent = model_config
        .map(|m| m.cli_agent.as_str())
        .unwrap_or("<none>");
    let model_computer_use = model_config
        .map(|m| m.computer_use_agent.as_str())
        .unwrap_or("<none>");

    let (anthropic_set, openai_set, google_set, open_router_set, aws_set, allow_warp_credits) =
        match api_keys {
            Some(k) => (
                !k.anthropic.is_empty(),
                !k.openai.is_empty(),
                !k.google.is_empty(),
                !k.open_router.is_empty(),
                k.aws_credentials.is_some(),
                k.allow_use_of_warp_credits,
            ),
            None => (false, false, false, false, false, false),
        };

    let task_count = task_context.map(|tc| tc.tasks.len()).unwrap_or(0);
    let conversation_id = metadata
        .map(|m| m.conversation_id.as_str())
        .unwrap_or("<none>");
    let forked_from = metadata
        .map(|m| m.forked_from_conversation_id.as_str())
        .unwrap_or("<none>");
    let parent_agent_id = metadata
        .map(|m| m.parent_agent_id.as_str())
        .unwrap_or("<none>");
    let agent_name = metadata.map(|m| m.agent_name.as_str()).unwrap_or("<none>");

    let input_variant = request
        .input
        .as_ref()
        .and_then(|i| i.r#type.as_ref())
        .map(|t| match t {
            api::request::input::Type::UserInputs(_) => "UserInputs",
            api::request::input::Type::QueryWithCannedResponse(_) => "QueryWithCannedResponse",
            api::request::input::Type::AutoCodeDiffQuery(_) => "AutoCodeDiffQuery",
            api::request::input::Type::ResumeConversation(_) => "ResumeConversation",
            api::request::input::Type::InitProjectRules(_) => "InitProjectRules",
            api::request::input::Type::GeneratePassiveSuggestions(_) => {
                "GeneratePassiveSuggestions"
            }
            api::request::input::Type::CreateNewProject(_) => "CreateNewProject",
            api::request::input::Type::CloneRepository(_) => "CloneRepository",
            api::request::input::Type::CodeReview(_) => "CodeReview",
            api::request::input::Type::SummarizeConversation(_) => "SummarizeConversation",
            api::request::input::Type::CreateEnvironment(_) => "CreateEnvironment",
            api::request::input::Type::FetchReviewComments(_) => "FetchReviewComments",
            api::request::input::Type::StartFromAmbientRunPrompt(_) => "StartFromAmbientRunPrompt",
            api::request::input::Type::InvokeSkill(_) => "InvokeSkill",
            #[allow(deprecated)]
            api::request::input::Type::UserQuery(_) => "UserQuery(deprecated)",
            #[allow(deprecated)]
            api::request::input::Type::ToolCallResult(_) => "ToolCallResult(deprecated)",
        })
        .unwrap_or("<none>");

    let has_existing_suggestions = request.existing_suggestions.is_some();
    let has_mcp_context = request.mcp_context.is_some();

    log::info!(
        "[byok-debug] multi-agent request shape: \
            model.base={model_base} model.coding={model_coding} \
            model.cli_agent={model_cli_agent} model.computer_use={model_computer_use} \
            api_keys{{anthropic={anthropic_set}, openai={openai_set}, google={google_set}, \
            open_router={open_router_set}, aws={aws_set}, allow_warp_credits={allow_warp_credits}}} \
            tasks={task_count} conversation_id=\"{conversation_id}\" \
            forked_from=\"{forked_from}\" parent_agent_id=\"{parent_agent_id}\" \
            agent_name=\"{agent_name}\" input={input_variant} \
            existing_suggestions={has_existing_suggestions} mcp_context={has_mcp_context}"
    );
}

fn get_supported_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_tools = vec![
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
        api::ToolType::ReadMcpResource,
        api::ToolType::CallMcpTool,
        api::ToolType::InitProject,
        api::ToolType::OpenCodeReview,
        api::ToolType::RunShellCommand,
        api::ToolType::SuggestNewConversation,
        api::ToolType::Subagent,
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::ReadDocuments,
        api::ToolType::CreateDocuments,
        api::ToolType::EditDocuments,
        api::ToolType::SuggestPrompt,
    ];

    if FeatureFlag::ConversationsAsContext.is_enabled() {
        supported_tools.push(api::ToolType::FetchConversation);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_tools.extend(&[
                api::ToolType::ReadFiles,
                api::ToolType::ApplyFileDiffs,
                api::ToolType::SearchCodebase,
            ]);

            if FeatureFlag::ArtifactCommand.is_enabled() {
                supported_tools.push(api::ToolType::UploadFileArtifact);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            // Remote session with a known host — enable tools that route
            // through RemoteServerClient. The host_id is only populated
            // after a successful connection handshake, so its presence is a
            // sufficient proxy for client availability.
            // SearchCodebase remains disabled (follow-up work).
            supported_tools.extend(&[api::ToolType::ReadFiles, api::ToolType::ApplyFileDiffs]);
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {
            // Feature flag off or not yet connected — no remote tools.
        }
    }

    if FeatureFlag::AgentModeComputerUse.is_enabled() && params.computer_use_enabled {
        supported_tools.extend(&[api::ToolType::UseComputer]);
        supported_tools.extend(&[api::ToolType::RequestComputerUse])
    }

    if FeatureFlag::PRCommentsSlashCommand.is_enabled() {
        supported_tools.push(api::ToolType::InsertReviewComments);
    }

    if FeatureFlag::ListSkills.is_enabled() {
        supported_tools.push(api::ToolType::ReadSkill);
    }

    if params.orchestration_enabled {
        supported_tools.push(if FeatureFlag::OrchestrationV2.is_enabled() {
            api::ToolType::StartAgentV2
        } else {
            api::ToolType::StartAgent
        });
        supported_tools.push(api::ToolType::SendMessageToAgent);
    }

    if FeatureFlag::AskUserQuestion.is_enabled() && params.ask_user_question_enabled {
        supported_tools.push(api::ToolType::AskUserQuestion);
    }

    supported_tools
}

fn get_supported_cli_agent_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_cli_agent_tools = vec![
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
    ];

    if FeatureFlag::TransferControlTool.is_enabled() {
        supported_cli_agent_tools.push(api::ToolType::TransferShellCommandControlToUser);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_cli_agent_tools
                .extend(&[api::ToolType::ReadFiles, api::ToolType::SearchCodebase]);
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            supported_cli_agent_tools.push(api::ToolType::ReadFiles);
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {}
    }

    supported_cli_agent_tools
}

#[cfg(test)]
#[path = "impl_tests.rs"]
mod tests;
