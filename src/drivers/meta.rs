//! Meta Model API adapter for response continuation IDs.

use async_trait::async_trait;
use everruns_contracts::driver_registry::{
    ChatDriver, DiscoveredModel, LlmCallConfig, LlmResponse, LlmResponseStream, Message,
};
use everruns_contracts::error::Result;
use everruns_contracts::{BearerAuth, DriverRegistry, Provider, ProviderEndpoint};
use everruns_drivers::meta::MetaChatDriver;
use std::sync::Arc;

#[derive(Debug)]
struct MetaResponseIdDriver(MetaChatDriver);

impl MetaResponseIdDriver {
    fn accepted_config(config: &LlmCallConfig) -> LlmCallConfig {
        let mut config = config.clone();
        // Meta can return `gen-` IDs while its previous_response_id parameter
        // accepts only `resp_` IDs. Replaying the transcript preserves context
        // for those turns without discarding valid stateful continuations.
        if config
            .previous_response_id
            .as_deref()
            .is_some_and(|id| !id.starts_with("resp_"))
        {
            config.previous_response_id = None;
        }
        config
    }
}

#[async_trait]
impl ChatDriver for MetaResponseIdDriver {
    async fn chat_completion_stream(
        &self,
        endpoint: &ProviderEndpoint,
        messages: Vec<Message>,
        config: &LlmCallConfig,
    ) -> Result<LlmResponseStream> {
        self.0
            .chat_completion_stream(endpoint, messages, &Self::accepted_config(config))
            .await
    }

    async fn chat_completion_non_streaming(
        &self,
        endpoint: &ProviderEndpoint,
        messages: Vec<Message>,
        config: &LlmCallConfig,
    ) -> Result<LlmResponse> {
        self.0
            .chat_completion_non_streaming(endpoint, messages, &Self::accepted_config(config))
            .await
    }

    fn supports_native_non_streaming(&self) -> bool {
        self.0.supports_native_non_streaming()
    }

    fn supports_stateful_responses(&self) -> bool {
        self.0.supports_stateful_responses()
    }

    fn supports_parallel_tool_calls(&self, model: &str) -> bool {
        self.0.supports_parallel_tool_calls(model)
    }

    async fn list_models(
        &self,
        endpoint: &ProviderEndpoint,
    ) -> Result<Option<Vec<DiscoveredModel>>> {
        self.0.list_models(endpoint).await
    }
}

pub fn register_driver(registry: &mut DriverRegistry) {
    let mut descriptor = everruns_drivers::meta::descriptor();
    descriptor.provider = Some(Arc::new(|config| {
        Provider::new(
            config.provider.clone(),
            MetaResponseIdDriver(MetaChatDriver::new()),
        )
        .base_url(
            config
                .base_url
                .as_deref()
                .unwrap_or("https://api.meta.ai/v1"),
        )
        .auth(BearerAuth::new(config.api_key.clone().unwrap_or_default()))
    }));
    registry.register_descriptor(descriptor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_contracts::{
        DriverId, LlmCallConfig, Message, MessageRole, ProviderConfig, ProviderEndpoint,
    };
    use serde_json::{Value, json};

    #[tokio::test]
    async fn meta_replays_transcript_for_non_response_ids_and_chains_valid_ids() {
        for (id, should_chain) in [("gen-1792196545-example", false), ("resp_123", true)] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}/v1", server.server_addr());
            let request = std::thread::spawn(move || {
                let mut request = server.recv().unwrap();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let terminal = json!({
                    "type": "response.completed", "sequence_number": 2,
                    "response": {
                        "id": "resp_reply", "object": "response", "created_at": 0,
                        "model": "muse-spark-1.3", "status": "completed",
                        "output": [{"type": "message", "id": "msg", "status": "completed",
                            "role": "assistant", "content": []}]
                    }
                });
                let wire = format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}}\n\ndata: {terminal}\n\n"
                );
                let mut response = tiny_http::Response::from_string(wire);
                response.add_header(
                    tiny_http::Header::from_bytes("content-type", "text/event-stream").unwrap(),
                );
                request.respond(response).unwrap();
                serde_json::from_str::<Value>(&body).unwrap()
            });
            let mut registry = DriverRegistry::new();
            register_driver(&mut registry);
            let driver = registry
                .create_chat_driver(
                    &ProviderConfig::new(DriverId::Meta)
                        .with_api_key("synthetic-key")
                        .with_base_url(base_url),
                )
                .unwrap();
            let mut config = LlmCallConfig::new("muse-spark-1.3");
            config.previous_response_id = Some(id.into());
            let messages = vec![
                Message::text(MessageRole::System, "rules"),
                Message::text(MessageRole::User, "old question"),
                Message::text(MessageRole::Assistant, "prior answer"),
                Message::text(MessageRole::User, "new question"),
            ];
            let response = driver
                .chat_completion(&ProviderEndpoint::default(), messages, &config)
                .await
                .unwrap();
            assert_eq!(response.text, "answer");
            let body = request.join().unwrap();
            if should_chain {
                assert_eq!(body["previous_response_id"], id);
                assert_eq!(body["input"].as_array().unwrap().len(), 1);
            } else {
                assert!(body.get("previous_response_id").is_none(), "{body}");
                assert_eq!(body["input"].as_array().unwrap().len(), 3);
            }
        }
    }
}
