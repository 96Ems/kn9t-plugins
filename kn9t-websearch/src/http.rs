//! HTTP client with native-tls for enterprise proxy compatibility.

use std::sync::Arc;
use ureq::Agent;

/// Creates an Agent configured with native-tls (uses OS certificate store).
/// This is necessary for environments with TLS-intercepting proxies (enterprise networks).
pub fn agent() -> Agent {
    let tls = native_tls::TlsConnector::new().expect("failed to create TLS connector");
    ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .build()
}
