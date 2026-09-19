use everruns_core::IntegrationPlugin;
use everruns_integrations_parallel::CAPABILITY_PLUGINS;

#[test]
fn parallel_search_plugin_is_published() {
    let plugins: Vec<&IntegrationPlugin> = CAPABILITY_PLUGINS.iter().collect();
    assert!(
        plugins.iter().any(|plugin| {
            let capability = (plugin.factory)();
            capability.id() == "parallel_search"
        }),
        "expected parallel_search integration plugin to be published"
    );
}
