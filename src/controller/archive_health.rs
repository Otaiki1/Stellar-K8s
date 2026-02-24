//! History Archive Health Check Module
//!
//! Provides async health checking for Stellar history archive URLs.
//! Used to verify archives are reachable before starting validator nodes.

use crate::error::{Error, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, warn};

/// Result of checking multiple history archive URLs
#[derive(Debug, Clone)]
pub struct ArchiveHealthResult {
    /// URLs that passed the health check
    pub healthy_urls: Vec<String>,
    /// URLs that failed with their error messages
    pub unhealthy_urls: Vec<(String, String)>,
    /// True if all URLs are healthy
    pub all_healthy: bool,
    /// True if at least one URL is healthy
    pub any_healthy: bool,
}

impl ArchiveHealthResult {
    /// Create a new result from check outcomes
    pub fn new(healthy: Vec<String>, unhealthy: Vec<(String, String)>) -> Self {
        let all_healthy = unhealthy.is_empty() && !healthy.is_empty();
        let any_healthy = !healthy.is_empty();

        Self {
            healthy_urls: healthy,
            unhealthy_urls: unhealthy,
            all_healthy,
            any_healthy,
        }
    }

    pub fn summary(&self) -> String {
        if self.healthy_urls.is_empty() && self.unhealthy_urls.is_empty() {
            "No archives configured".to_string()
        } else if self.all_healthy {
            format!("All {} archive(s) healthy", self.healthy_urls.len())
        } else if self.any_healthy {
            format!(
                "{} healthy, {} unhealthy archive(s)",
                self.healthy_urls.len(),
                self.unhealthy_urls.len()
            )
        } else {
            format!("All {} archive(s) unhealthy", self.unhealthy_urls.len())
        }
    }

    /// Get detailed error messages for unhealthy archives
    pub fn error_details(&self) -> String {
        self.unhealthy_urls
            .iter()
            .map(|(url, err)| format!("  - {url}: {err}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Check health of a single history archive URL
///
/// Tries the following endpoints in order:
/// 1. HEAD request to `.well-known/stellar-history.json` (lightweight)
/// 2. GET request to root `/` (fallback)
async fn check_single_archive(client: &Client, url: &str, timeout: Duration) -> Result<()> {
    let base_url = url.trim_end_matches('/');

    // Try the standard Stellar history metadata endpoint first
    let metadata_url = format!("{base_url}/.well-known/stellar-history.json");

    debug!("Checking archive health: {}", metadata_url);

    match client.head(&metadata_url).timeout(timeout).send().await {
        Ok(resp) if resp.status().is_success() => {
            debug!("Archive healthy (metadata endpoint): {}", url);
            return Ok(());
        }
        Ok(resp) => {
            debug!(
                "Metadata endpoint returned {}, trying root: {}",
                resp.status(),
                url
            );
        }
        Err(e) => {
            debug!("Metadata endpoint failed ({}), trying root: {}", e, url);
        }
    }

    // Fallback to root endpoint
    match client.head(base_url).timeout(timeout).send().await {
        Ok(resp) if resp.status().is_success() => {
            debug!("Archive healthy (root endpoint): {}", url);
            Ok(())
        }
        Ok(resp) => {
            let msg = format!("Archive returned HTTP {}", resp.status());
            warn!("{}: {}", url, msg);
            Err(Error::ArchiveHealthCheckError(msg))
        }
        Err(e) => {
            let msg = format!("Connection failed: {e}");
            warn!("{}: {}", url, msg);
            Err(Error::HttpError(e))
        }
    }
}

/// Check health of multiple history archive URLs in parallel
///
/// # Arguments
/// * `urls` - List of archive URLs to check
/// * `timeout` - Timeout per URL check (default: 10 seconds)
///
/// # Returns
/// `ArchiveHealthResult` with details of healthy and unhealthy archives
pub async fn check_history_archive_health(
    urls: &[String],
    timeout: Option<Duration>,
) -> Result<ArchiveHealthResult> {
    if urls.is_empty() {
        debug!("No archive URLs to check, skipping health check");
        return Ok(ArchiveHealthResult::new(vec![], vec![]));
    }

    let timeout = timeout.unwrap_or(Duration::from_secs(10));

    // Create HTTP client with reasonable defaults
    let client = Client::builder()
        .timeout(timeout)
        .user_agent("stellar-k8s-operator/0.1.0")
        .build()
        .map_err(Error::HttpError)?;

    // Check all URLs in parallel
    let checks: Vec<_> = urls
        .iter()
        .map(|url| check_single_archive(&client, url, timeout))
        .collect();

    let results = futures::future::join_all(checks).await;

    // Categorize results
    let mut healthy = Vec::new();
    let mut unhealthy = Vec::new();

    for (url, result) in urls.iter().zip(results.into_iter()) {
        match result {
            Ok(()) => healthy.push(url.clone()),
            Err(e) => unhealthy.push((url.clone(), e.to_string())),
        }
    }

    let health_result = ArchiveHealthResult::new(healthy, unhealthy);

    debug!("Archive health check complete: {}", health_result.summary());

    Ok(health_result)
}

/// Calculate exponential backoff delay for retry attempts
///
/// # Arguments
/// * `attempt` - Current retry attempt number (0-indexed)
/// * `base_delay_secs` - Base delay in seconds (default: 15)
/// * `max_delay_secs` - Maximum delay cap in seconds (default: 300 = 5 minutes)
///
/// # Returns
/// Duration to wait before next retry
pub fn calculate_backoff(
    attempt: u32,
    base_delay_secs: Option<u64>,
    max_delay_secs: Option<u64>,
) -> Duration {
    let base = base_delay_secs.unwrap_or(15);
    let max = max_delay_secs.unwrap_or(300);

    // Exponential: base * 2^attempt, capped at max
    let delay_secs = base.saturating_mul(2_u64.saturating_pow(attempt.min(5)));
    let capped_delay = delay_secs.min(max);

    Duration::from_secs(capped_delay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_backoff_calculation() {
        // Attempt 0: 15 seconds
        assert_eq!(calculate_backoff(0, None, None), Duration::from_secs(15));

        // Attempt 1: 30 seconds
        assert_eq!(calculate_backoff(1, None, None), Duration::from_secs(30));

        // Attempt 2: 60 seconds
        assert_eq!(calculate_backoff(2, None, None), Duration::from_secs(60));

        // Attempt 3: 120 seconds
        assert_eq!(calculate_backoff(3, None, None), Duration::from_secs(120));

        // Attempt 4: 240 seconds
        assert_eq!(calculate_backoff(4, None, None), Duration::from_secs(240));

        // Attempt 5+: capped at 300 seconds (5 minutes)
        assert_eq!(calculate_backoff(5, None, None), Duration::from_secs(300));
        assert_eq!(calculate_backoff(10, None, None), Duration::from_secs(300));
    }

    #[test]
    fn test_health_result_summary() {
        let result = ArchiveHealthResult::new(vec!["http://archive1.com".to_string()], vec![]);
        assert!(result.all_healthy);
        assert!(result.any_healthy);
        assert_eq!(result.summary(), "All 1 archive(s) healthy");

        let result = ArchiveHealthResult::new(
            vec!["http://archive1.com".to_string()],
            vec![("http://archive2.com".to_string(), "timeout".to_string())],
        );
        assert!(!result.all_healthy);
        assert!(result.any_healthy);
        assert_eq!(result.summary(), "1 healthy, 1 unhealthy archive(s)");

        let result = ArchiveHealthResult::new(
            vec![],
            vec![("http://archive1.com".to_string(), "timeout".to_string())],
        );
        assert!(!result.all_healthy);
        assert!(!result.any_healthy);
        assert_eq!(result.summary(), "All 1 archive(s) unhealthy");
    }

    #[test]
    fn test_health_result_empty() {
        let result = ArchiveHealthResult::new(vec![], vec![]);
        assert!(!result.all_healthy);
        assert!(!result.any_healthy);
        assert_eq!(result.summary(), "No archives configured");
    }

    /// Test that a reachable archive with valid metadata returns healthy status
    #[tokio::test]
    async fn test_reachable_archive_healthy() {
        let mock_server = MockServer::start().await;

        // Mock successful response to stellar-history.json
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(result.all_healthy);
        assert!(result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 1);
        assert_eq!(result.unhealthy_urls.len(), 0);
        assert_eq!(result.healthy_urls[0], mock_server.uri());
    }

    /// Test that an unreachable archive is marked as unhealthy
    #[tokio::test]
    async fn test_unreachable_archive_unhealthy() {
        // Use an invalid URL that will fail to connect
        let urls = vec!["http://localhost:1".to_string()];
        let result = check_history_archive_health(&urls, Some(Duration::from_millis(100)))
            .await
            .unwrap();

        assert!(!result.all_healthy);
        assert!(!result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 0);
        assert_eq!(result.unhealthy_urls.len(), 1);
        assert_eq!(result.unhealthy_urls[0].0, "http://localhost:1");
        // Error message should contain connection-related info
        assert!(result.unhealthy_urls[0].1.contains("HTTP"));
    }

    /// Test that an archive with missing metadata but working root is flagged as degraded
    #[tokio::test]
    async fn test_archive_degraded_stale_metadata() {
        let mock_server = MockServer::start().await;

        // Mock 404 for stellar-history.json (stale/missing metadata)
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        // Mock successful response to root endpoint (archive is still reachable)
        Mock::given(method("HEAD"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        // Archive is considered healthy (reachable via root), though metadata is missing
        assert!(result.all_healthy);
        assert!(result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 1);
        assert_eq!(result.unhealthy_urls.len(), 0);
    }

    /// Test that completely unreachable archive (both endpoints fail) is marked unhealthy
    #[tokio::test]
    async fn test_archive_both_endpoints_fail() {
        let mock_server = MockServer::start().await;

        // Mock 500 for stellar-history.json
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        // Mock 500 for root endpoint
        Mock::given(method("HEAD"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(!result.all_healthy);
        assert!(!result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 0);
        assert_eq!(result.unhealthy_urls.len(), 1);
        assert!(result.unhealthy_urls[0].1.contains("HTTP 500"));
    }

    /// Test that the check respects configurable timeout
    #[tokio::test]
    async fn test_timeout_respected() {
        let mock_server = MockServer::start().await;

        // Mock a delayed response (2 seconds)
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];

        // Test with short timeout (100ms) - should fail
        let result = check_history_archive_health(&urls, Some(Duration::from_millis(100)))
            .await
            .unwrap();

        assert!(!result.all_healthy);
        assert_eq!(result.unhealthy_urls.len(), 1);
        // Error should indicate timeout or connection issue
        assert!(
            result.unhealthy_urls[0].1.contains("HTTP")
                || result.unhealthy_urls[0].1.contains("timeout")
                || result.unhealthy_urls[0].1.contains("Connection")
        );
    }

    /// Test that the check works with long enough timeout
    #[tokio::test]
    async fn test_timeout_sufficient() {
        let mock_server = MockServer::start().await;

        // Mock a slightly delayed response (100ms)
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(100)))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];

        // Test with sufficient timeout (5 seconds) - should succeed
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(result.all_healthy);
        assert_eq!(result.healthy_urls.len(), 1);
        assert_eq!(result.unhealthy_urls.len(), 0);
    }

    /// Test multiple archives with mixed health status
    #[tokio::test]
    async fn test_multiple_archives_mixed_health() {
        let mock_server1 = MockServer::start().await;
        let mock_server2 = MockServer::start().await;

        // First archive: healthy
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server1)
            .await;

        // Second archive: unhealthy (500 error)
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server2)
            .await;

        Mock::given(method("HEAD"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server2)
            .await;

        let urls = vec![mock_server1.uri(), mock_server2.uri()];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(!result.all_healthy);
        assert!(result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 1);
        assert_eq!(result.unhealthy_urls.len(), 1);
        assert_eq!(result.summary(), "1 healthy, 1 unhealthy archive(s)");
    }

    /// Test error_details formatting for unhealthy archives
    #[tokio::test]
    async fn test_error_details_formatting() {
        let mock_server = MockServer::start().await;

        // Mock unhealthy archive
        Mock::given(method("HEAD"))
            .and(path("/.well-known/stellar-history.json"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&mock_server)
            .await;

        Mock::given(method("HEAD"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&mock_server)
            .await;

        let urls = vec![mock_server.uri()];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        let error_details = result.error_details();
        assert!(!error_details.is_empty());
        assert!(error_details.contains(&mock_server.uri()));
        assert!(error_details.contains("HTTP 503"));
    }

    /// Test empty URL list handling
    #[tokio::test]
    async fn test_empty_url_list() {
        let urls: Vec<String> = vec![];
        let result = check_history_archive_health(&urls, Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(!result.all_healthy);
        assert!(!result.any_healthy);
        assert_eq!(result.healthy_urls.len(), 0);
        assert_eq!(result.unhealthy_urls.len(), 0);
        assert_eq!(result.summary(), "No archives configured");
    }
}
