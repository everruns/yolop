//! Generic connector (user connection) support for yolop.
//!
//! Connectors wrap upstream [`everruns_core::connector::Connector`]
//! implementations so sandbox and integration capabilities (Daytona today;
//! E2B and others later) can resolve credentials lazily at tool time through
//! [`YolopConnectionResolver`].

mod capability;
mod catalog;
mod resolver;
mod store;

pub(crate) use capability::{CONNECTORS_CAPABILITY_ID, ConnectorsCapability};
pub(crate) use catalog::ConnectionCatalog;
pub(crate) use resolver::YolopConnectionResolver;
pub(crate) use store::{ConnectionStore, default_connections_path};
