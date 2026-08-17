//! In-process local inference.
//!
//! The `ollama` provider is an OpenAI-compatible endpoint that happens to live
//! on loopback: it needs a second process installed, running, and holding the
//! weights. This driver removes that process. The engine ([`mistralrs`]) is
//! linked into yolop, downloads its own weights from Hugging Face on first use,
//! and answers `chat_completion_stream` without a socket in between.
//!
//! Weights are *not* embedded — a multi-GB tensor file cannot ride in a crate.
//! What is embedded is the runtime, so "install a daemon" becomes "wait for a
//! download the first time".
//!
//! Compiled only under the `local-inference` feature; see the feature comment
//! in `Cargo.toml` for why it is off by default.

use async_trait::async_trait;
use everruns_provider::driver_registry::{
    ChatDriver, DriverConfig, DriverRegistry, LlmCallConfig, LlmCompletionMetadata, LlmMessage,
    LlmMessageRole, LlmResponseStream, LlmStreamEvent,
};
use everruns_provider::error::{AgentLoopError, LlmErrorKind, Result as EverrunsResult};
use everruns_provider::tool_types::{ToolCall, ToolDefinition};
use futures::stream::Stream;
use mistralrs::{
    ChatCompletionChunkResponse, ChunkChoice, Delta, Function, GgufModelBuilder, IsqBits, Model,
    ModelBuilder, RequestBuilder, Response, TextMessageRole, Tool, ToolType,
};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

pub use crate::drivers::LOCAL_DRIVER_ID;

use crate::models::GGUF_SEPARATOR;

/// Engines are keyed by model spec and shared process-wide. Loading is minutes
/// and gigabytes, and the registry builds a fresh driver per request, so the
/// cache has to outlive the driver instance rather than sit inside it.
///
/// The entries are `&'static` rather than `Arc` because the engine's streaming
/// handle borrows the model: an `Arc` would make every response stream
/// self-referential. Leaking is honest here — a loaded engine is deliberately
/// process-lifetime (nothing evicts it, and re-loading costs minutes and
/// gigabytes), so the reference is never dangling and never reclaimed anyway.
type EngineCache = Arc<Mutex<HashMap<String, &'static Model>>>;

pub fn register_driver(registry: &mut DriverRegistry) {
    let engines: EngineCache = Arc::new(Mutex::new(HashMap::new()));
    registry.register_external(LOCAL_DRIVER_ID, move |_config: &DriverConfig| {
        Box::new(LocalChatDriver {
            engines: engines.clone(),
        })
    });
}

pub struct LocalChatDriver {
    engines: EngineCache,
}

impl LocalChatDriver {
    /// Load-or-reuse the engine for `spec`. Held across the await so two
    /// concurrent turns on the same model wait for one load instead of racing
    /// two multi-gigabyte downloads.
    async fn engine(&self, spec: &str) -> EverrunsResult<&'static Model> {
        let mut engines = self.engines.lock().await;
        if let Some(engine) = engines.get(spec) {
            return Ok(engine);
        }

        // Load only from the local store. Letting the engine fetch its own
        // weights would turn the first turn into a silent multi-gigabyte stall;
        // `yolop models pull` owns that wait and shows progress for it.
        if !crate::models::is_installed(spec) {
            return Err(missing_weights_error(spec));
        }
        let (repo, gguf_file) = crate::models::split_spec(spec);
        let dir = crate::models::repo_dir(repo).map_err(|err| load_error(spec, err))?;
        let dir = dir.to_string_lossy().to_string();

        tracing::info!(model = spec, path = %dir, "loading local inference engine");
        let model = match gguf_file {
            Some(file) => GgufModelBuilder::new(dir, vec![file.to_string()])
                .build()
                .await
                .map_err(|err| load_error(spec, err))?,
            None => ModelBuilder::new(dir)
                .with_auto_isq(IsqBits::Eight)
                .build()
                .await
                .map_err(|err| load_error(spec, err))?,
        };

        let engine: &'static Model = Box::leak(Box::new(model));
        engines.insert(spec.to_string(), engine);
        Ok(engine)
    }
}

#[async_trait]
impl ChatDriver for LocalChatDriver {
    async fn chat_completion_stream(
        &self,
        // 0.18 passes the resolved endpoint per call instead of baking it into
        // the driver. In-process inference has no endpoint to honor.
        _endpoint: &everruns_provider::runtime_provider::ProviderEndpoint,
        messages: Vec<LlmMessage>,
        config: &LlmCallConfig,
    ) -> EverrunsResult<LlmResponseStream> {
        let engine = self.engine(&config.model).await?;

        let mut request = RequestBuilder::new();
        for message in &messages {
            request = append_message(request, message);
        }
        if !config.tools.is_empty() {
            request = request.set_tools(convert_tools(&config.tools));
        }
        if let Some(max_tokens) = config.max_tokens {
            request = request.set_sampler_max_len(max_tokens as usize);
        }

        let stream = engine
            .stream_chat_request(request)
            .await
            .map_err(|err| stream_error(&config.model, err))?;

        Ok(Box::pin(into_llm_events(
            Box::pin(stream),
            config.model.clone(),
        )))
    }
}

/// Replay one transcript message into the request.
///
/// Tool results carry their `tool_call_id` so the chat template can pair them
/// with the call that produced them; an assistant turn that *made* calls is
/// replayed as plain text, because the local template only needs the pairing
/// from the tool side.
fn append_message(request: RequestBuilder, message: &LlmMessage) -> RequestBuilder {
    let text = message.content.to_text();
    match message.role {
        LlmMessageRole::System => request.add_message(TextMessageRole::System, text),
        LlmMessageRole::User => request.add_message(TextMessageRole::User, text),
        LlmMessageRole::Assistant => request.add_message(TextMessageRole::Assistant, text),
        LlmMessageRole::Tool => match &message.tool_call_id {
            Some(id) => request.add_tool_message(text, id),
            None => request.add_message(TextMessageRole::Tool, text),
        },
    }
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<Tool> {
    tools
        .iter()
        .map(|tool| Tool {
            tp: ToolType::Function,
            function: Function {
                name: tool.name().to_string(),
                description: Some(tool.description().to_string()),
                // `Function::parameters` is a JSON object map, so a schema that
                // is not an object (or is absent) becomes "no parameters"
                // rather than a malformed tool the template would reject.
                parameters: tool
                    .parameters()
                    .as_object()
                    .map(|obj| obj.clone().into_iter().collect()),
            },
        })
        .collect()
}

/// Adapt the engine's `Response` stream to the host's event stream.
///
/// Tool calls arrive on the delta like OpenAI's, but the host wants them as one
/// terminal `ToolCalls` event, so they are accumulated and flushed with the
/// finish reason. `pending` exists because a single chunk can produce several
/// host events (trailing text, then tool calls, then `Done`).
fn into_llm_events(
    mut stream: Pin<Box<dyn Stream<Item = Response> + Send>>,
    model: String,
) -> impl Stream<Item = EverrunsResult<LlmStreamEvent>> + Send {
    let mut pending: VecDeque<LlmStreamEvent> = VecDeque::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut finished = false;

    futures::stream::poll_fn(move |cx| {
        loop {
            if let Some(event) = pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(event)));
            }
            if finished {
                return std::task::Poll::Ready(None);
            }

            match stream.as_mut().poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => {
                    // Stream ended without a terminal chunk; still close the
                    // turn so the caller is not left waiting on `Done`.
                    finished = true;
                    pending.extend(terminal_events(
                        &model,
                        std::mem::take(&mut tool_calls),
                        finish_reason.take(),
                        None,
                    ));
                }
                std::task::Poll::Ready(Some(response)) => match response {
                    Response::Chunk(ChatCompletionChunkResponse { choices, usage, .. }) => {
                        let Some(ChunkChoice {
                            delta:
                                Delta {
                                    content,
                                    tool_calls: delta_tool_calls,
                                    ..
                                },
                            finish_reason: chunk_finish,
                            ..
                        }) = choices.into_iter().next()
                        else {
                            continue;
                        };

                        if let Some(text) = content.filter(|text| !text.is_empty()) {
                            pending.push_back(LlmStreamEvent::TextDelta(text));
                        }
                        if let Some(calls) = delta_tool_calls {
                            tool_calls.extend(calls.into_iter().map(|call| {
                                ToolCall {
                                    id: call.id,
                                    name: call.function.name,
                                    // The engine emits arguments as a JSON string;
                                    // an unparseable one becomes Null so the host
                                    // validator reports a bad call rather than the
                                    // driver panicking mid-stream.
                                    arguments: serde_json::from_str(&call.function.arguments)
                                        .unwrap_or(serde_json::Value::Null),
                                }
                            }));
                        }
                        if chunk_finish.is_some() {
                            finish_reason = chunk_finish;
                            finished = true;
                            pending.extend(terminal_events(
                                &model,
                                std::mem::take(&mut tool_calls),
                                finish_reason.take(),
                                usage.map(|usage| {
                                    (usage.prompt_tokens as u32, usage.completion_tokens as u32)
                                }),
                            ));
                        }
                    }
                    Response::ModelError(message, _) => {
                        finished = true;
                        return std::task::Poll::Ready(Some(Err(inference_error(&model, message))));
                    }
                    Response::ValidationError(err) | Response::InternalError(err) => {
                        finished = true;
                        return std::task::Poll::Ready(Some(Err(inference_error(
                            &model,
                            err.to_string(),
                        ))));
                    }
                    // Non-streaming and completion variants never appear on a
                    // `stream_chat_request` stream.
                    _ => continue,
                },
            }
        }
    })
}

fn terminal_events(
    model: &str,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<(u32, u32)>,
) -> Vec<LlmStreamEvent> {
    let mut events = Vec::new();
    let has_tool_calls = !tool_calls.is_empty();
    if has_tool_calls {
        events.push(LlmStreamEvent::ToolCalls(tool_calls));
    }

    let (prompt_tokens, completion_tokens) = match usage {
        Some((prompt, completion)) => (Some(prompt), Some(completion)),
        None => (None, None),
    };
    events.push(LlmStreamEvent::Done(Box::new(LlmCompletionMetadata {
        model: Some(model.to_string()),
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens
            .zip(completion_tokens)
            .map(|(prompt, completion)| prompt + completion),
        // Tool calls win over whatever the engine reported. A local chat
        // template that stops to call a tool still finishes the sequence
        // normally, so the engine says "stop"; the host reads this field to
        // tell a finished answer from a turn that is waiting on tools.
        finish_reason: if has_tool_calls {
            Some("tool_calls".to_string())
        } else {
            finish_reason
        },
        ..Default::default()
    })));
    events
}

/// Absent weights are a setup step, not a failure to explain — the message is
/// the command that fixes it.
fn missing_weights_error(spec: &str) -> AgentLoopError {
    AgentLoopError::llm_kind(
        LlmErrorKind::Unavailable,
        format!(
            "local model '{spec}' is not downloaded. Run `yolop models pull {spec}` first \
             (several GB), then retry. `yolop models list` shows what is already on disk."
        ),
    )
}

fn load_error(spec: &str, err: impl std::fmt::Display) -> AgentLoopError {
    // `Unavailable`, not `InvalidRequest`: a failed load is the local
    // equivalent of an unreachable provider (missing weights, no disk, no
    // network), not a malformed request the caller could fix by retrying.
    AgentLoopError::llm_kind(
        LlmErrorKind::Unavailable,
        format!(
            "failed to load local model '{spec}': {err}. Model specs are Hugging Face repos \
             ('Qwen/Qwen3-8B') or a GGUF file inside one \
             ('unsloth/Qwen3-8B-GGUF{GGUF_SEPARATOR}Qwen3-8B-Q4_K_M.gguf')."
        ),
    )
}

fn stream_error(model: &str, err: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::llm_kind(
        LlmErrorKind::Other,
        format!("local inference request failed for '{model}': {err}"),
    )
}

fn inference_error(model: &str, message: String) -> AgentLoopError {
    AgentLoopError::llm_kind(
        LlmErrorKind::Other,
        format!("local inference failed for '{model}': {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_provider::driver_registry::LlmMessageContent;
    use mistralrs::RequestLike;

    fn message(role: LlmMessageRole, text: &str) -> LlmMessage {
        LlmMessage {
            role,
            content: LlmMessageContent::Text(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            phase: None,
            thinking: None,
            thinking_signature: None,
        }
    }

    #[test]
    fn gguf_specs_split_repo_from_file() {
        let spec = "unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf";
        assert_eq!(
            spec.split_once(GGUF_SEPARATOR),
            Some(("unsloth/Qwen3-8B-GGUF", "Qwen3-8B-Q4_K_M.gguf"))
        );
        assert_eq!("Qwen/Qwen3-8B".split_once(GGUF_SEPARATOR), None);
    }

    #[test]
    fn tool_results_keep_their_call_id() {
        // The chat template pairs a tool result with its call by id; losing it
        // silently turns a tool turn into an anonymous message.
        let mut tool = message(LlmMessageRole::Tool, "42");
        tool.tool_call_id = Some("call_7".to_string());
        let request = append_message(RequestBuilder::new(), &tool);

        let messages = request.messages_ref();
        assert_eq!(messages.len(), 1);
        let id = messages[0]
            .get("tool_call_id")
            .and_then(|value| value.as_ref().left())
            .map(String::as_str);
        assert_eq!(id, Some("call_7"));
    }

    #[test]
    fn terminal_events_report_tool_calls_before_done() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        }];
        let events = terminal_events("Qwen/Qwen3-8B", calls, None, Some((10, 5)));

        assert!(matches!(events[0], LlmStreamEvent::ToolCalls(_)));
        let LlmStreamEvent::Done(meta) = &events[1] else {
            panic!("expected Done, got {:?}", events[1]);
        };
        assert_eq!(meta.total_tokens, Some(15));
        // A local engine reports no finish reason of its own when it stops to
        // call a tool, so the driver supplies the one the host expects.
        assert_eq!(meta.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn tool_calls_override_the_engines_finish_reason() {
        // The engine reports a normal stop even when the template emitted tool
        // calls; reporting "stop" upstream would read as a finished answer.
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        }];
        let events = terminal_events("Qwen/Qwen3-8B", calls, Some("stop".to_string()), None);
        let LlmStreamEvent::Done(meta) = &events[1] else {
            panic!("expected Done, got {:?}", events[1]);
        };
        assert_eq!(meta.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn terminal_events_omit_tool_calls_when_there_are_none() {
        let events = terminal_events("Qwen/Qwen3-8B", Vec::new(), Some("stop".to_string()), None);
        assert_eq!(events.len(), 1);
        let LlmStreamEvent::Done(meta) = &events[0] else {
            panic!("expected Done");
        };
        assert_eq!(meta.finish_reason.as_deref(), Some("stop"));
        assert_eq!(meta.total_tokens, None);
    }
}
