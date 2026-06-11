//! Setup overlay state machine.
//!
//! The `SetupStep` enum (defined in the parent module) is the overlay's state;
//! these `impl App` methods are its transitions. The overlay owns its own
//! keyboard handling — `handle_setup_key` dispatches per step so provider,
//! credential, base-URL, model, and effort input never echo through the normal
//! chat composer. Provider/model discovery helpers live here too because they
//! exist only to feed the picker steps. Rendering of the overlay lives in
//! `render.rs`; this module is state and transitions only.

use super::*;

impl App {
    pub(crate) fn start_setup(&mut self) {
        self.setup = Some(SetupStep::Provider {
            selected: self.current_provider_index(),
        });
    }

    pub(crate) fn start_first_run_setup(&mut self) {
        self.start_setup();
    }

    pub(crate) fn start_model_setup(&mut self) {
        let provider = self.current_provider_name();
        self.open_model_step(&provider);
    }

    pub(crate) fn start_model_setup_with_arg(&mut self, raw: &str) {
        let spec = raw.trim();
        if spec.is_empty() {
            self.start_model_setup();
            return;
        }
        let provider = self.current_provider_name();
        self.request_model_discovery(&provider);
        let options = self.model_options(&provider);
        let selected = model_index_for_label(spec, &options);
        let custom = if options
            .get(selected)
            .and_then(|option| option.spec.as_deref())
            == Some(spec)
        {
            None
        } else {
            Some(spec.to_string())
        };
        self.setup = Some(SetupStep::PickModel {
            provider,
            selected,
            custom,
            error: None,
        });
    }

    pub(crate) fn start_effort_setup(&mut self, raw: &str) {
        let raw = raw.trim();
        let selected = effort_index(raw).unwrap_or_else(|| self.current_effort_index());
        self.setup = Some(SetupStep::PickEffort {
            selected,
            error: if raw.is_empty() || effort_index(raw).is_some() {
                None
            } else {
                Some(format!("unknown effort: {raw}"))
            },
        });
    }

    pub(crate) fn current_effort_index(&self) -> usize {
        let label = self.model.provider_label();
        if !label.starts_with("openai/")
            && !label.starts_with("openrouter/")
            && !label.starts_with("custom/")
        {
            return effort_index("medium").unwrap_or(0);
        }
        label
            .split_whitespace()
            .nth(1)
            .and_then(effort_index)
            .unwrap_or_else(|| effort_index("medium").unwrap_or(0))
    }

    pub(crate) fn current_provider_name(&self) -> String {
        self.model
            .provider_label()
            .split('/')
            .next()
            .unwrap_or("openai")
            .trim()
            .to_string()
    }

    pub(crate) fn current_provider_index(&self) -> usize {
        let provider = self.current_provider_name();
        PROVIDER_OPTIONS
            .iter()
            .position(|option| option.name == provider)
            .unwrap_or(0)
    }

    pub(crate) fn provider_label(name: &str) -> &'static str {
        PROVIDER_OPTIONS
            .iter()
            .find(|option| option.name == name)
            .map(|option| option.label)
            .unwrap_or("provider")
    }

    pub(crate) fn provider_env_names(provider: &str) -> &'static [&'static str] {
        match provider {
            "openai" => &["OPENAI_API_KEY"],
            "anthropic" => &["ANTHROPIC_API_KEY"],
            "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "openrouter" => &["OPENROUTER_API_KEY"],
            "ollama" => &["OLLAMA_BASE_URL", "OLLAMA_API_KEY"],
            "custom" => &["CUSTOM_API_KEY"],
            _ => &[],
        }
    }

    /// Connection status shown in the provider picker. `true` means the
    /// provider can be used right away (setup may jump straight to model
    /// selection); the string is the short status appended to the row hint.
    pub(crate) fn provider_status(
        settings: &crate::settings::Settings,
        provider: &str,
    ) -> (bool, String) {
        match provider {
            "llmsim" | "ollama" => (true, "✓ no key needed".to_string()),
            "custom" => {
                if crate::runtime::custom_base_url(settings).is_some() {
                    (true, "✓ base URL set".to_string())
                } else {
                    (false, "needs base URL".to_string())
                }
            }
            _ => {
                if let Some(env) = Self::detected_env_var(provider) {
                    (true, format!("✓ {env}"))
                } else if settings.has_token(provider) {
                    (true, "✓ saved key".to_string())
                } else {
                    (false, "needs API key".to_string())
                }
            }
        }
    }

    pub(crate) fn detected_env_var(provider: &str) -> Option<&'static str> {
        Self::provider_env_names(provider)
            .iter()
            .copied()
            .find(|name| {
                std::env::var(name)
                    .map(|value| !value.is_empty())
                    .unwrap_or(false)
            })
    }

    pub(crate) fn credential_options(provider: &str) -> Vec<CredentialOption> {
        let env_names = Self::provider_env_names(provider);
        // The custom endpoint treats a key as optional: most local servers
        // accept any bearer token, so the first option proceeds with the env
        // key when present and a placeholder otherwise.
        let env_label = if provider == "custom" {
            "Continue without key".to_string()
        } else {
            match env_names {
                [] => "Use environment".to_string(),
                [one] => format!("Use {one} from environment"),
                many => format!("Use {} from environment", many.join(" / ")),
            }
        };
        let env_hint = Self::detected_env_var(provider)
            .map(|name| format!("{name} detected"))
            .unwrap_or_else(|| {
                if provider == "custom" {
                    "fine for endpoints without auth".to_string()
                } else {
                    "not detected yet".to_string()
                }
            });
        vec![
            CredentialOption {
                id: CredentialAction::UseEnv,
                label: env_label,
                hint: env_hint,
            },
            CredentialOption {
                id: CredentialAction::PasteKey,
                label: "Paste API key".to_string(),
                hint: "saved to settings.toml".to_string(),
            },
            CredentialOption {
                id: CredentialAction::Skip,
                label: "Skip for now".to_string(),
                hint: "leave setup unchanged".to_string(),
            },
            CredentialOption {
                id: CredentialAction::ClearSaved,
                label: "Clear saved key".to_string(),
                hint: "remove this provider token".to_string(),
            },
        ]
    }

    /// Options for the model picker: models discovered live from the
    /// provider's models API when available, otherwise the curated
    /// fallback list.
    pub(crate) fn model_options(&self, provider: &str) -> Vec<ModelOption> {
        match self.model_catalog.get(provider) {
            Some(options) => options.clone(),
            None => Self::fallback_model_options(provider),
        }
    }

    /// Curated static list, used until (or instead of) live discovery —
    /// e.g. before the models API responds, when the provider does not
    /// support listing, or when the query fails.
    pub(crate) fn fallback_model_options(provider: &str) -> Vec<ModelOption> {
        let mut models = match provider {
            "openai" => vec![
                ModelOption {
                    spec: Some("gpt-5.5".to_string()),
                    label: "gpt-5.5".to_string(),
                    hint: "frontier model for complex coding".to_string(),
                },
                ModelOption {
                    spec: Some("gpt-5.4".to_string()),
                    label: "gpt-5.4".to_string(),
                    hint: "strong everyday model".to_string(),
                },
                ModelOption {
                    spec: Some("gpt-5.4-mini".to_string()),
                    label: "gpt-5.4-mini".to_string(),
                    hint: "fast and cost-efficient".to_string(),
                },
                ModelOption {
                    spec: Some("gpt-5.3-codex".to_string()),
                    label: "gpt-5.3-codex".to_string(),
                    hint: "coding-optimized model".to_string(),
                },
                ModelOption {
                    spec: Some("gpt-5.2".to_string()),
                    label: "gpt-5.2".to_string(),
                    hint: "optimized for long-running agents".to_string(),
                },
            ],
            "anthropic" => vec![
                ModelOption {
                    spec: Some("claude-sonnet-4-5".to_string()),
                    label: "claude-sonnet-4-5".to_string(),
                    hint: "best default Claude model".to_string(),
                },
                ModelOption {
                    spec: Some("claude-opus-4-5".to_string()),
                    label: "claude-opus-4-5".to_string(),
                    hint: "more capable for complex work".to_string(),
                },
                ModelOption {
                    spec: Some("claude-haiku-4-5".to_string()),
                    label: "claude-haiku-4-5".to_string(),
                    hint: "fast answers".to_string(),
                },
                ModelOption {
                    spec: Some("claude-sonnet-4-6".to_string()),
                    label: "claude-sonnet-4-6".to_string(),
                    hint: "newer Sonnet option".to_string(),
                },
                ModelOption {
                    spec: Some("claude-fable-5".to_string()),
                    label: "claude-fable-5".to_string(),
                    hint: "most powerful Claude model".to_string(),
                },
            ],
            "google" => vec![
                ModelOption {
                    spec: Some("gemini-2.5-flash".to_string()),
                    label: "gemini-2.5-flash".to_string(),
                    hint: "fast Gemini default".to_string(),
                },
                ModelOption {
                    spec: Some("gemini-2.5-pro".to_string()),
                    label: "gemini-2.5-pro".to_string(),
                    hint: "more capable Gemini model".to_string(),
                },
            ],
            "openrouter" => vec![
                ModelOption {
                    spec: Some("openai/gpt-5.2".to_string()),
                    label: "openai/gpt-5.2".to_string(),
                    hint: "default OpenRouter model".to_string(),
                },
                ModelOption {
                    spec: Some("nvidia/nemotron-3-super-120b-a12b high".to_string()),
                    label: "nvidia/nemotron-3-super-120b-a12b".to_string(),
                    hint: "reasoning model through OpenRouter".to_string(),
                },
                ModelOption {
                    spec: Some("anthropic/claude-sonnet-4-5".to_string()),
                    label: "anthropic/claude-sonnet-4-5".to_string(),
                    hint: "Claude through OpenRouter".to_string(),
                },
            ],
            "ollama" => vec![ModelOption {
                spec: Some("llama3.2".to_string()),
                label: "llama3.2".to_string(),
                hint: "local default model".to_string(),
            }],
            // No preset list exists for an arbitrary endpoint; only the
            // trailing "Custom..." free-form entry is offered.
            "custom" => Vec::new(),
            _ => vec![ModelOption {
                spec: Some("llmsim-yolop".to_string()),
                label: "llmsim-yolop".to_string(),
                hint: "offline demo model".to_string(),
            }],
        };
        models.push(custom_model_option());
        models
    }

    pub(crate) fn is_fetching_models(&self, provider: &str) -> bool {
        self.model_fetches_in_flight.contains(provider)
    }

    /// Kick off a background fetch of the provider's models API, if one is
    /// not already cached or in flight. The result lands on `models_rx`
    /// and is applied between frames; the picker shows the fallback list
    /// (plus a loading hint) in the meantime.
    pub(crate) fn request_model_discovery(&mut self, provider: &str) {
        if !self.model_discovery_enabled
            || provider == "llmsim"
            || self.model_catalog.contains_key(provider)
            || self.model_fetches_in_flight.contains(provider)
        {
            return;
        }
        // Prefer the live provider choice (it carries any custom base URL);
        // fall back to the provider's defaults when picking across providers.
        let choice = if self.current_provider_name() == provider {
            self.model.provider_choice()
        } else {
            match crate::runtime::ProviderChoice::default_for_provider_name(provider) {
                Ok(choice) => choice,
                Err(_) => return,
            }
        };
        self.model_fetches_in_flight.insert(provider.to_string());
        let tx = self.models_tx.clone();
        let settings = self.settings.clone();
        let provider_name = provider.to_string();
        tokio::spawn(async move {
            let result = match tokio::time::timeout(
                Duration::from_secs(10),
                crate::capabilities::model_discovery::discover_provider_models(
                    &choice,
                    &settings.snapshot(),
                ),
            )
            .await
            {
                Ok(Ok(Some(models))) => Ok(Some(model_options_from_discovered(models))),
                Ok(Ok(None)) => Ok(None),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("models API request timed out".to_string()),
            };
            let _ = tx.send(ModelDiscovery {
                provider: provider_name,
                result,
            });
        });
    }

    /// Apply a finished models API fetch: cache the options and re-anchor
    /// an open picker for that provider on the refreshed list.
    pub(crate) fn apply_model_discovery(&mut self, discovery: ModelDiscovery) {
        self.model_fetches_in_flight.remove(&discovery.provider);
        match discovery.result {
            // > 1 because the list always ends with the "Custom..." entry.
            Ok(Some(options)) if options.len() > 1 => {
                self.model_catalog
                    .insert(discovery.provider.clone(), options);
                if let Some(SetupStep::PickModel {
                    provider,
                    custom,
                    error,
                    ..
                }) = self.setup.clone()
                    && provider == discovery.provider
                    && custom.is_none()
                {
                    let selected = model_index_for_label(
                        &self.model.model_id(),
                        &self.model_options(&provider),
                    );
                    self.setup = Some(SetupStep::PickModel {
                        provider,
                        selected,
                        custom,
                        error,
                    });
                }
            }
            // Listing unsupported (or empty): cache the curated fallback so
            // reopening the picker doesn't re-query an API that can't answer.
            // Errors are deliberately not cached — the next picker open retries.
            Ok(_) => {
                self.model_catalog.insert(
                    discovery.provider.clone(),
                    Self::fallback_model_options(&discovery.provider),
                );
            }
            Err(mut err) => {
                if let Some(SetupStep::PickModel {
                    provider, error, ..
                }) = self.setup.as_mut()
                    && *provider == discovery.provider
                {
                    err.truncate(120);
                    *error = Some(format!("model list unavailable: {err}"));
                }
            }
        }
    }

    pub(crate) async fn handle_setup_key(&mut self, key: KeyEvent) {
        let Some(step) = self.setup.clone() else {
            return;
        };
        match step {
            SetupStep::Provider { selected } => {
                self.handle_provider_key(key, selected).await;
            }
            SetupStep::BaseUrlInput { value, .. } => {
                self.handle_base_url_key(key, value).await;
            }
            SetupStep::Credential {
                provider, selected, ..
            } => {
                self.handle_credential_key(key, provider, selected).await;
            }
            SetupStep::TokenInput {
                provider, token, ..
            } => {
                self.handle_token_key(key, provider, token).await;
            }
            SetupStep::PickModel {
                provider,
                selected,
                custom,
                ..
            } => {
                self.handle_model_key(key, provider, selected, custom).await;
            }
            SetupStep::PickEffort { selected, .. } => {
                self.handle_effort_key(key, selected).await;
            }
        }
    }

    pub(crate) async fn handle_provider_key(&mut self, key: KeyEvent, selected: usize) {
        match key.code {
            KeyCode::Esc => {
                self.setup = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.setup = Some(SetupStep::Provider {
                    selected: selected.saturating_sub(1),
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.setup = Some(SetupStep::Provider {
                    selected: (selected + 1).min(PROVIDER_OPTIONS.len().saturating_sub(1)),
                });
            }
            KeyCode::Char('c') => {
                self.open_provider_config(selected);
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                if let Some(index) = digit_index(ch, PROVIDER_OPTIONS.len()) {
                    self.confirm_provider(index).await;
                }
            }
            KeyCode::Enter => {
                self.confirm_provider(selected).await;
            }
            _ => {}
        }
    }

    /// Enter on a provider row. Already-connected providers switch
    /// immediately and jump straight to model selection; the rest go
    /// through credential (or base URL) configuration first.
    pub(crate) async fn confirm_provider(&mut self, selected: usize) {
        let option = PROVIDER_OPTIONS
            .get(selected)
            .unwrap_or(&PROVIDER_OPTIONS[0]);
        if option.name == "llmsim" {
            match self.run_setup_command(Some("provider llmsim")).await {
                Ok(()) => {
                    self.setup = None;
                    self.push_system("setup complete: offline demo mode".into());
                }
                Err(error) => {
                    self.setup = Some(SetupStep::Provider { selected });
                    self.push_system(format!("setup failed: {error}"));
                }
            }
            return;
        }

        let (connected, _) = Self::provider_status(&self.settings.snapshot(), option.name);
        if !connected {
            self.open_provider_config(selected);
            return;
        }

        if option.name == "custom" {
            // Best-effort switch: it fails harmlessly when no model is saved
            // yet, and the model step right after sets provider + model
            // atomically via `provider custom <model>`.
            let _ = self.run_setup_command(Some("provider custom")).await;
            self.open_model_step("custom");
            return;
        }

        match self
            .run_setup_command(Some(&format!("provider {}", option.name)))
            .await
        {
            Ok(()) => self.open_model_step(option.name),
            Err(error) => {
                // A connected-looking provider that still fails to switch
                // (stale key, unreachable endpoint) lands on the credential
                // step where the user can fix it.
                self.setup = Some(SetupStep::Credential {
                    provider: option.name.to_string(),
                    selected: 0,
                    error: Some(error),
                });
            }
        }
    }

    /// `c` on a provider row: configure credentials (or the endpoint base
    /// URL for the custom provider) even when already connected — e.g. to
    /// replace or clear a saved key.
    pub(crate) fn open_provider_config(&mut self, selected: usize) {
        let option = PROVIDER_OPTIONS
            .get(selected)
            .unwrap_or(&PROVIDER_OPTIONS[0]);
        match option.name {
            "llmsim" => {}
            "custom" => self.open_base_url_step(),
            _ => {
                self.setup = Some(SetupStep::Credential {
                    provider: option.name.to_string(),
                    selected: 0,
                    error: None,
                });
            }
        }
    }

    pub(crate) fn open_base_url_step(&mut self) {
        let value = crate::runtime::custom_base_url(&self.settings.snapshot()).unwrap_or_default();
        self.setup = Some(SetupStep::BaseUrlInput { value, error: None });
    }

    /// Open the model picker for `provider`, preselecting the active model
    /// and kicking off live discovery. The custom provider has no curated
    /// list, so until (or unless) discovery fills the catalog its picker
    /// shows only the free-form "Custom..." entry.
    pub(crate) fn open_model_step(&mut self, provider: &str) {
        self.request_model_discovery(provider);
        let selected = model_index_for_label(&self.model.model_id(), &self.model_options(provider));
        self.setup = Some(SetupStep::PickModel {
            provider: provider.to_string(),
            selected,
            custom: None,
            error: None,
        });
    }

    /// Prefill for the custom provider's free-form model input: the active
    /// model when already on the custom provider, else the saved one.
    pub(crate) fn custom_model_prefill(&self) -> String {
        if self.current_provider_name() == "custom" {
            return self.model.model_id();
        }
        self.settings
            .snapshot()
            .model_for("custom")
            .unwrap_or_default()
            .to_string()
    }

    pub(crate) async fn handle_base_url_key(&mut self, key: KeyEvent, mut value: String) {
        match key.code {
            KeyCode::Esc => {
                self.setup = Some(SetupStep::Provider {
                    selected: PROVIDER_OPTIONS
                        .iter()
                        .position(|option| option.name == "custom")
                        .unwrap_or(0),
                });
            }
            KeyCode::Enter => {
                let trimmed = value.trim().to_string();
                if trimmed.is_empty() {
                    self.setup = Some(SetupStep::BaseUrlInput {
                        value,
                        error: Some(
                            "enter a base URL like http://localhost:8000/v1, or press Esc"
                                .to_string(),
                        ),
                    });
                    return;
                }
                match self
                    .run_setup_command(Some(&format!("url custom {trimmed}")))
                    .await
                {
                    Ok(()) => {
                        self.setup = Some(SetupStep::Credential {
                            provider: "custom".to_string(),
                            selected: 0,
                            error: None,
                        });
                    }
                    Err(error) => {
                        self.setup = Some(SetupStep::BaseUrlInput {
                            value,
                            error: Some(error),
                        });
                    }
                }
            }
            KeyCode::Backspace => {
                value.pop();
                self.setup = Some(SetupStep::BaseUrlInput { value, error: None });
            }
            KeyCode::Char(_) => {
                if let KeyCode::Char(ch) = normalize_printable_key(key).code {
                    value.push(ch);
                }
                self.setup = Some(SetupStep::BaseUrlInput { value, error: None });
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_credential_key(
        &mut self,
        key: KeyEvent,
        provider: String,
        selected: usize,
    ) {
        let options = Self::credential_options(&provider);
        match key.code {
            KeyCode::Esc => {
                self.setup = Some(SetupStep::Provider {
                    selected: PROVIDER_OPTIONS
                        .iter()
                        .position(|option| option.name == provider)
                        .unwrap_or(0),
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.setup = Some(SetupStep::Credential {
                    provider,
                    selected: selected.saturating_sub(1),
                    error: None,
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.setup = Some(SetupStep::Credential {
                    provider,
                    selected: (selected + 1).min(options.len().saturating_sub(1)),
                    error: None,
                });
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                if let Some(index) = digit_index(ch, options.len()) {
                    self.confirm_credential(provider, index).await;
                }
            }
            KeyCode::Enter => {
                self.confirm_credential(provider, selected).await;
            }
            _ => {}
        }
    }

    pub(crate) async fn confirm_credential(&mut self, provider: String, selected: usize) {
        let options = Self::credential_options(&provider);
        let action = options
            .get(selected)
            .map(|option| option.id)
            .unwrap_or(CredentialAction::UseEnv);
        match action {
            CredentialAction::UseEnv => {
                // The custom provider can't be switched to until a model is
                // chosen, so the validation switch is deferred to the model
                // step (its key is optional anyway).
                if provider == "custom" {
                    self.open_model_step("custom");
                    return;
                }
                match self
                    .run_setup_command(Some(&format!("provider {provider}")))
                    .await
                {
                    Ok(()) => self.open_model_step(&provider),
                    Err(error) => {
                        self.setup = Some(SetupStep::Credential {
                            provider,
                            selected,
                            error: Some(error),
                        });
                    }
                }
            }
            CredentialAction::PasteKey => {
                self.setup = Some(SetupStep::TokenInput {
                    provider,
                    token: String::new(),
                    error: None,
                });
            }
            CredentialAction::Skip => {
                self.setup = None;
                self.push_system("setup skipped".into());
            }
            CredentialAction::ClearSaved => {
                let result = self
                    .run_setup_command(Some(&format!("token {provider} clear")))
                    .await;
                match result {
                    Ok(()) => {
                        self.setup = None;
                        self.push_system(format!(
                            "setup complete: cleared saved key for {provider}"
                        ));
                    }
                    Err(error) => {
                        self.setup = Some(SetupStep::Credential {
                            provider,
                            selected,
                            error: Some(error),
                        });
                    }
                }
            }
        }
    }

    pub(crate) async fn handle_token_key(
        &mut self,
        key: KeyEvent,
        provider: String,
        mut token: String,
    ) {
        match key.code {
            KeyCode::Esc => {
                self.setup = Some(SetupStep::Credential {
                    provider,
                    selected: 1,
                    error: None,
                });
            }
            KeyCode::Enter => {
                let trimmed = token.trim().to_string();
                if trimmed.is_empty() {
                    self.setup = Some(SetupStep::TokenInput {
                        provider,
                        token,
                        error: Some("API key is empty — paste a key, or press Esc".to_string()),
                    });
                    return;
                }
                let save_arg = format!("token {provider} {trimmed}");
                if let Err(error) = self.run_setup_command(Some(&save_arg)).await {
                    self.setup = Some(SetupStep::TokenInput {
                        provider,
                        token: String::new(),
                        error: Some(error),
                    });
                    return;
                }
                // Custom defers the provider switch to the model step (no
                // model is known yet); other providers validate the new key
                // by switching now.
                if provider == "custom" {
                    self.open_model_step("custom");
                    return;
                }
                match self
                    .run_setup_command(Some(&format!("provider {provider}")))
                    .await
                {
                    Ok(()) => self.open_model_step(&provider),
                    Err(error) => {
                        self.setup = Some(SetupStep::TokenInput {
                            provider,
                            token: String::new(),
                            error: Some(error),
                        });
                    }
                }
            }
            KeyCode::Backspace => {
                token.pop();
                self.setup = Some(SetupStep::TokenInput {
                    provider,
                    token,
                    error: None,
                });
            }
            KeyCode::Char(_) => {
                if let KeyCode::Char(ch) = normalize_printable_key(key).code {
                    token.push(ch);
                }
                self.setup = Some(SetupStep::TokenInput {
                    provider,
                    token,
                    error: None,
                });
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_model_key(
        &mut self,
        key: KeyEvent,
        provider: String,
        selected: usize,
        custom: Option<String>,
    ) {
        let options = self.model_options(&provider);
        if let Some(mut value) = custom {
            match key.code {
                KeyCode::Esc => {
                    self.setup = Some(SetupStep::PickModel {
                        provider,
                        selected,
                        custom: None,
                        error: None,
                    });
                }
                KeyCode::Enter => {
                    let value = value.trim().to_string();
                    if value.is_empty() {
                        self.setup = Some(SetupStep::PickModel {
                            provider,
                            selected,
                            custom: Some(String::new()),
                            error: Some("paste a model id, or press Esc to go back".to_string()),
                        });
                        return;
                    }
                    self.save_model_and_finish(&provider, &value, selected)
                        .await;
                }
                KeyCode::Backspace => {
                    value.pop();
                    self.setup = Some(SetupStep::PickModel {
                        provider,
                        selected,
                        custom: Some(value),
                        error: None,
                    });
                }
                KeyCode::Char(_) => {
                    if let KeyCode::Char(ch) = normalize_printable_key(key).code {
                        value.push(ch);
                    }
                    self.setup = Some(SetupStep::PickModel {
                        provider,
                        selected,
                        custom: Some(value),
                        error: None,
                    });
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.setup = Some(SetupStep::Provider {
                    selected: PROVIDER_OPTIONS
                        .iter()
                        .position(|option| option.name == provider)
                        .unwrap_or(0),
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.setup = Some(SetupStep::PickModel {
                    provider,
                    selected: selected.saturating_sub(1),
                    custom: None,
                    error: None,
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.setup = Some(SetupStep::PickModel {
                    provider,
                    selected: (selected + 1).min(options.len().saturating_sub(1)),
                    custom: None,
                    error: None,
                });
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                if let Some(index) = digit_index(ch, options.len()) {
                    self.confirm_model(provider, index).await;
                }
            }
            KeyCode::Enter => {
                self.confirm_model(provider, selected).await;
            }
            _ => {}
        }
    }

    pub(crate) async fn confirm_model(&mut self, provider: String, selected: usize) {
        let options = self.model_options(&provider);
        let Some(option) = options.get(selected) else {
            self.setup = Some(SetupStep::PickModel {
                provider,
                selected: 0,
                custom: None,
                error: None,
            });
            return;
        };
        if let Some(spec) = &option.spec {
            self.save_model_and_finish(&provider, spec, selected).await;
        } else {
            // "Custom..." free-form input. For the custom endpoint the
            // current/saved model is prefilled — it is the provider's only
            // memory of a model id.
            let prefill = if provider == "custom" {
                self.custom_model_prefill()
            } else {
                String::new()
            };
            self.setup = Some(SetupStep::PickModel {
                provider,
                selected,
                custom: Some(prefill),
                error: None,
            });
        }
    }

    pub(crate) async fn save_model_and_finish(
        &mut self,
        provider: &str,
        spec: &str,
        selected: usize,
    ) {
        // `model <spec>` resolves against the current provider. For the
        // custom endpoint the wizard reaches this step before any switch
        // could succeed (a first-time custom provider has no model yet), so
        // it switches provider and model atomically instead.
        let command = if provider == "custom" {
            format!("provider custom {spec}")
        } else {
            format!("model {spec}")
        };
        match self.run_setup_command(Some(&command)).await {
            Ok(()) => {
                self.setup = None;
                self.push_system(format!("setup complete: {}", self.model.provider_label()));
            }
            Err(error) => {
                self.setup = Some(SetupStep::PickModel {
                    provider: provider.to_string(),
                    selected,
                    custom: None,
                    error: Some(error),
                });
            }
        }
    }

    pub(crate) async fn handle_effort_key(&mut self, key: KeyEvent, selected: usize) {
        match key.code {
            KeyCode::Esc => {
                self.setup = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.setup = Some(SetupStep::PickEffort {
                    selected: selected.saturating_sub(1),
                    error: None,
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.setup = Some(SetupStep::PickEffort {
                    selected: (selected + 1).min(EFFORT_OPTIONS.len().saturating_sub(1)),
                    error: None,
                });
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                if let Some(index) = digit_index(ch, EFFORT_OPTIONS.len()) {
                    self.confirm_effort(index).await;
                }
            }
            KeyCode::Enter => {
                self.confirm_effort(selected).await;
            }
            _ => {}
        }
    }

    pub(crate) async fn confirm_effort(&mut self, selected: usize) {
        let Some(option) = EFFORT_OPTIONS.get(selected) else {
            self.setup = Some(SetupStep::PickEffort {
                selected: 0,
                error: None,
            });
            return;
        };
        match self
            .run_setup_command(Some(&format!("effort {}", option.value)))
            .await
        {
            Ok(()) => {
                self.setup = None;
                self.push_system(format!("setup complete: {}", self.model.provider_label()));
            }
            Err(error) => {
                self.setup = Some(SetupStep::PickEffort {
                    selected,
                    error: Some(error),
                });
            }
        }
    }

    pub(crate) async fn execute_setup_command(
        &mut self,
        arg: Option<&str>,
    ) -> Result<everruns_core::command::CommandResult, String> {
        let request = everruns_core::command::ExecuteCommandRequest {
            name: "setup".to_string(),
            arguments: arg.map(str::to_string),
            controls: None,
        };
        self.handles
            .runtime
            .execute_command(self.handles.session_id, request)
            .await
            .map_err(|err| err.to_string())
    }

    /// Execute an internal setup command. Successful mutations stay quiet so
    /// the user only sees the overlay and a single final completion line.
    pub(crate) async fn run_setup_command(&mut self, arg: Option<&str>) -> Result<(), String> {
        match self.execute_setup_command(arg).await {
            Ok(result) if result.success => Ok(()),
            Ok(result) => Err(result.message),
            Err(err) => Err(err),
        }
    }
}
