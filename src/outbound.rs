//! HTTP client for Microsoft endpoints (JWKS, Entra token endpoint).

use std::time::Duration;

use anyhow::{Context, Result};

use crate::settings::Settings;

/// Honours HTTP(S)_PROXY, and trusts the configured CA bundle (where the Docker image installs the corporate
/// root/issuing CAs) in addition to the built-in roots.
pub fn client(s: &Settings) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(10)).connect_timeout(Duration::from_secs(5));
    if let Some(path) = &s.ca_bundle {
        let pem = std::fs::read(path).with_context(|| format!("read CA bundle {path}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).with_context(|| format!("parse CA bundle {path}"))?;
        tracing::debug!(path, certs = certs.len(), "extra CA certificates loaded");
        for c in certs {
            b = b.add_root_certificate(c);
        }
    }
    Ok(b.build()?)
}
