//! Terminal-independent presentation for optional MCP discovery readiness.
use crate::runtime::mcp_readiness::McpDiscoveryStatus;

pub(super) fn line(name: &str, status: McpDiscoveryStatus) -> String {
    let status = match status {
        McpDiscoveryStatus::Pending => "pending discovery",
        McpDiscoveryStatus::Ready => "ready",
        McpDiscoveryStatus::AuthenticationRequired => "authentication required",
        McpDiscoveryStatus::Failed => "discovery failed",
    };
    format!("{name}: {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_states_are_distinct_in_the_transcript() {
        for (status, expected) in [
            (McpDiscoveryStatus::Pending, "docs: pending discovery"),
            (McpDiscoveryStatus::Ready, "docs: ready"),
            (
                McpDiscoveryStatus::AuthenticationRequired,
                "docs: authentication required",
            ),
            (McpDiscoveryStatus::Failed, "docs: discovery failed"),
        ] {
            assert_eq!(line("docs", status), expected);
        }
    }
}
