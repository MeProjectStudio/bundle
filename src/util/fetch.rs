use anyhow::{bail, Context, Result};
use reqwest::Client;

/// Build a shared reqwest client with a sensible user-agent and TLS.
fn make_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("mcpm/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

/// Fetch a URL and return its full body as bytes.
///
/// Follows redirects (up to the reqwest default limit).
pub async fn fetch_url(url: &str) -> Result<Vec<u8>> {
    let client = make_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("sending GET request to {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("GET {} returned HTTP {}", url, status);
    }

    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading response body from {}", url))?;

    Ok(bytes.to_vec())
}
