//! In-process local inference, temporarily disabled.
//!
//! The mistralrs/candle engine pulled zip 7.2.0 (security advisories fixed in
//! zip 9.0.0/9.0.5) with no fixed mistralrs release on crates.io. The driver
//! stays registered so `--provider local` still resolves, but every call
//! fails with a clear message. Weight downloads (hf-hub, no zip) still work
//! behind `local-inference`. Re-enable the engine once mistralrs publishes a
//! release on candle-core 0.11.1+ / zip 9.0.5+.

use async_trait::async_trait;
use everruns_contracts::driver_registry::{
    ChatDriver, DriverConfig, DriverRegistry, LlmCallConfig, LlmResponseStream,
};
use everruns_contracts::error::{AgentLoopError, LlmErrorKind, Result as EverrunsResult};
use everruns_contracts::message::Message as LlmMessage;

pub use crate::drivers::LOCAL_DRIVER_ID;

pub fn register_driver(registry: &mut DriverRegistry) {
    registry.register_external(LOCAL_DRIVER_ID, move |_config: &DriverConfig| {
        Box::new(LocalChatDriver)
    });
}

pub struct LocalChatDriver;

fn disabled() -> AgentLoopError {
    AgentLoopError::llm_kind(
        LlmErrorKind::Unavailable,
        "local inference is temporarily disabled: the mistralrs engine depends on zip 7.2.0 with unfixed security advisories; reinstall when upstream publishes a fixed release",
    )
}

#[async_trait]
impl ChatDriver for LocalChatDriver {
    async fn chat_completion_stream(
        &self,
        _endpoint: &everruns_contracts::runtime_provider::ProviderEndpoint,
        _messages: Vec<LlmMessage>,
        _config: &LlmCallConfig,
    ) -> EverrunsResult<LlmResponseStream> {
        Err(disabled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_driver_reports_temporary_disablement() {
        let err = disabled();
        assert_eq!(err.llm_error_kind(), Some(LlmErrorKind::Unavailable));
        assert!(
            err.to_string().contains("temporarily disabled"),
            "unexpected message: {err}"
        );
    }
}
