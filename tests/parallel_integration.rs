use everruns_core::IntegrationPlugin;
use everruns_integrations_parallel as _;

#[test]
fn parallel_search_plugin_is_registered() {
    let plugins: Vec<&IntegrationPlugin> = inventory::iter::<IntegrationPlugin>().collect();
    assert!(
        plugins.iter().any(|plugin| {
            let capability = (plugin.factory)();
            capability.id() == "parallel_search"
        }),
        "expected parallel_search integration plugin to be registered"
    );
}
