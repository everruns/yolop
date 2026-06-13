//! Registry of available connector providers.
//!
//! Yolop registers upstream [`Connector`] implementations here. The
//! catalog is the single place new sandbox backends (E2B, etc.) plug in.

use everruns_core::connector::{Connector, ConnectorRegistry, ConnectorValidation};
use everruns_integrations_daytona::connection::DaytonaConnector;
use std::collections::HashMap;
use std::sync::Arc;

use crate::connectors::store::{ConnectionStore, StoredConnection};

/// Describes one connector for model-facing tools and setup UI.
#[derive(Debug, Clone)]
pub struct ConnectorInfo {
    pub provider_id: String,
    pub display_name: String,
    pub description: String,
    pub icon: String,
    pub connection_type: everruns_core::connector::ConnectorType,
    pub form_schema: Option<everruns_core::connector::ConnectorFormSchema>,
    pub connected: bool,
}

pub struct ConnectionCatalog {
    providers: ConnectorRegistry,
}

impl ConnectionCatalog {
    pub fn with_defaults() -> Self {
        Self::builder().build()
    }

    /// Register an additional connector provider (e.g. future E2B integration).
    pub fn builder() -> ConnectionCatalogBuilder {
        ConnectionCatalogBuilder {
            providers: ConnectorRegistry::new(),
        }
    }
}

pub struct ConnectionCatalogBuilder {
    providers: ConnectorRegistry,
}

impl ConnectionCatalogBuilder {
    #[allow(dead_code)] // used by tests and future sandbox provider wiring
    pub fn register(mut self, provider: impl Connector + 'static) -> Self {
        self.providers.register(provider);
        self
    }

    pub fn build(mut self) -> ConnectionCatalog {
        if !self.providers.has("daytona") {
            self.providers.register(DaytonaConnector);
        }
        ConnectionCatalog {
            providers: self.providers,
        }
    }
}

impl ConnectionCatalog {
    #[allow(dead_code)]
    pub fn register(&mut self, provider: impl Connector + 'static) {
        self.providers.register(provider);
    }

    pub fn get(&self, provider_id: &str) -> Option<&Arc<dyn Connector>> {
        self.providers.get(provider_id)
    }

    pub fn list(&self) -> Vec<&Arc<dyn Connector>> {
        self.providers.list()
    }

    pub fn connector_info(
        &self,
        provider: &Arc<dyn Connector>,
        store: &ConnectionStore,
    ) -> ConnectorInfo {
        let provider_id = provider.provider_id().to_string();
        ConnectorInfo {
            provider_id: provider_id.clone(),
            display_name: provider.display_name().to_string(),
            description: provider.description().to_string(),
            icon: provider.icon().to_string(),
            connection_type: provider.connection_type(),
            form_schema: provider.form_schema(),
            connected: store.is_connected(&provider_id)
                || crate::connectors::resolver::env_credential(&provider_id).is_some(),
        }
    }

    pub fn list_connectors(&self, store: &ConnectionStore) -> Vec<ConnectorInfo> {
        let mut infos: Vec<_> = self
            .list()
            .into_iter()
            .map(|p| self.connector_info(p, store))
            .collect();
        infos.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        infos
    }

    pub async fn validate_and_store(
        &self,
        store: &ConnectionStore,
        provider_id: &str,
        fields: HashMap<String, String>,
    ) -> Result<ConnectorValidation, String> {
        let provider = self
            .get(provider_id)
            .ok_or_else(|| format!("unknown connector `{provider_id}`"))?;
        let validation = provider.validate_fields(&fields).await?;
        let stored_fields: std::collections::BTreeMap<String, String> =
            fields.into_iter().collect();
        store
            .save(
                provider_id,
                StoredConnection {
                    fields: stored_fields,
                    metadata: validation.provider_metadata.clone(),
                },
            )
            .map_err(|e| format!("failed to save connection: {e}"))?;
        Ok(validation)
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use everruns_core::connector::ConnectorType;

    struct StubProvider;

    #[async_trait::async_trait]
    impl Connector for StubProvider {
        fn provider_id(&self) -> &str {
            "stub"
        }
        fn display_name(&self) -> &str {
            "Stub"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn icon(&self) -> &str {
            "box"
        }
        fn connection_type(&self) -> ConnectorType {
            ConnectorType::ApiKey
        }
        fn form_schema(&self) -> Option<everruns_core::connector::ConnectorFormSchema> {
            None
        }
        async fn validate(&self, _credential: &str) -> Result<ConnectorValidation, String> {
            Ok(ConnectorValidation {
                provider_username: None,
                provider_metadata: None,
            })
        }
    }

    #[test]
    fn builder_registers_extra_providers() {
        let catalog = ConnectionCatalog::builder().register(StubProvider).build();
        assert!(catalog.get("stub").is_some());
        assert!(catalog.get("daytona").is_some());
    }
}
