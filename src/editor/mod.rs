//! Host editor integrations.
//!
//! The Agent Client Protocol (ACP) server that editors such as Zed drive over
//! stdio, and the `into` command that writes yolop's config into supported
//! editors.

pub mod acp;
pub mod into;
