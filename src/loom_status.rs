use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Config;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct LoomStatusReporter {
    api_url: String,
    host_header: String,
    token: String,
    public_url: String,
}

impl LoomStatusReporter {
    pub fn new(config: &Config) -> Self {
        Self {
            api_url: config.loom_api_url.trim_end_matches('/').to_string(),
            host_header: config.loom_host.trim().to_string(),
            token: config.loom_status_token.clone(),
            public_url: config.public_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.api_url.is_empty() && !self.host_header.is_empty() && !self.token.is_empty()
    }

    pub async fn report(
        &self,
        repo_url: &str,
        commit_sha: &str,
        run_id: &str,
        state: &str,
        description: &str,
    ) -> Result<(), String> {
        if !self.enabled() || commit_sha.is_empty() {
            return Ok(());
        }
        let (owner, repository) = loom_repository(repo_url, &self.host_header)
            .ok_or_else(|| "repository is not hosted by Loom".to_string())?;
        let (connect_host, port, base_path) = parse_http_url(&self.api_url)
            .ok_or_else(|| "ANVIL_LOOM_API_URL must use http://".to_string())?;
        let path = format!(
            "{base_path}/r/{owner}/{repository}/statuses/{commit_sha}",
            base_path = base_path.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "state": state,
            "context": "ci/anvil",
            "description": description,
            "target_url": format!("{}/run/{run_id}", self.public_url)
        })
        .to_string();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}",
            host = self.host_header,
            token = self.token,
            length = body.len()
        );

        let response = tokio::time::timeout(CALLBACK_TIMEOUT, async {
            let mut stream = TcpStream::connect((connect_host.as_str(), port))
                .await
                .map_err(|error| error.to_string())?;
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|error| error.to_string())?;
            let mut bytes = Vec::with_capacity(512);
            stream
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(bytes)
        })
        .await
        .map_err(|_| "Loom status callback timed out".to_string())??;

        let status_line = String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        if !status_line.contains(" 200 ") {
            return Err(format!("Loom status callback returned {status_line}"));
        }
        Ok(())
    }
}

fn parse_http_url(value: &str) -> Option<(String, u16, String)> {
    let rest = value.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, format!("/{path}")))
}

fn loom_repository(repo_url: &str, expected_host: &str) -> Option<(String, String)> {
    let rest = repo_url
        .strip_prefix("https://")
        .or_else(|| repo_url.strip_prefix("http://"))?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .to_ascii_lowercase();
    let expected = expected_host.split(':').next()?.to_ascii_lowercase();
    if host != expected {
        return None;
    }

    let mut segments: Vec<&str> = path
        .split(['?', '#'])
        .next()?
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.first().copied() == Some("git") {
        segments.remove(0);
    }
    if segments.len() != 2 {
        return None;
    }
    let owner = segments[0];
    let repository = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if !safe_segment(owner) || !safe_segment(repository) {
        return None;
    }
    Some((owner.to_string(), repository.to_string()))
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_only_exact_loom_clone_urls() {
        assert_eq!(
            loom_repository("https://git.w33d.xyz/git/acme/widgets.git", "git.w33d.xyz"),
            Some(("acme".to_string(), "widgets".to_string()))
        );
        assert_eq!(
            loom_repository("https://github.com/acme/widgets.git", "git.w33d.xyz"),
            None
        );
        assert_eq!(
            loom_repository("https://git.w33d.xyz/git/../../etc", "git.w33d.xyz"),
            None
        );
    }

    #[tokio::test]
    async fn posts_a_bearer_authenticated_status_to_the_loom_vhost() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let received = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let mut config = Config::dev();
        config.loom_api_url = format!("http://{address}");
        config.loom_status_token = "status-secret".to_string();
        let reporter = LoomStatusReporter::new(&config);

        reporter
            .report(
                "https://git.w33d.xyz/git/acme/widgets.git",
                "abc1234",
                "run-1",
                "success",
                "2 steps passed",
            )
            .await
            .unwrap();

        let request = received.await.unwrap();
        assert!(request.starts_with("POST /r/acme/widgets/statuses/abc1234 HTTP/1.1"));
        assert!(request.contains("Host: git.w33d.xyz"));
        assert!(request.contains("Authorization: Bearer status-secret"));
        assert!(request.contains(r#""context":"ci/anvil""#));
        assert!(request.contains("https://ci.w33d.xyz/run/run-1"));
    }
}
