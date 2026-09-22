use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::AdapterError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Http,
    #[cfg_attr(not(feature = "https"), allow(dead_code))]
    Https,
}

#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
    query: Option<String>,
}

impl Endpoint {
    pub(crate) fn parse(value: &str, adapter: &str) -> Result<Self, AdapterError> {
        let invalid = || {
            AdapterError::Configuration(format!(
                "{adapter} endpoint must be an absolute HTTP(S) URL"
            ))
        };
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return Err(invalid());
        }
        let (scheme, remainder) = value.split_once("://").ok_or_else(invalid)?;
        let scheme = match scheme {
            "http" => Scheme::Http,
            "https" => {
                #[cfg(not(feature = "https"))]
                return Err(AdapterError::Configuration(format!(
                    "{adapter} endpoint uses HTTPS, but safe-llm-adapter was built without the 'https' feature"
                )));
                #[cfg(feature = "https")]
                Scheme::Https
            }
            _ => return Err(invalid()),
        };
        if remainder.contains('#') {
            return Err(AdapterError::Configuration(format!(
                "{adapter} endpoint must not contain a URL fragment"
            )));
        }

        let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        let target = &remainder[authority_end..];
        if authority.is_empty() || authority.contains('@') {
            return Err(invalid());
        }
        let (host, explicit_port) = parse_authority(authority).ok_or_else(invalid)?;
        let port = explicit_port.unwrap_or(match scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        });
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (
                if path.is_empty() { "/" } else { path },
                Some(query.to_string()),
            ),
            None => (if target.is_empty() { "/" } else { target }, None),
        };

        Ok(Self {
            scheme,
            host: host.to_string(),
            port,
            path: path.to_string(),
            query,
        })
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn set_path(&mut self, path: &str) {
        self.path = path.to_string();
    }

    fn request_target(&self) -> String {
        match &self.query {
            Some(query) => format!("{}?{query}", self.path),
            None => self.path.clone(),
        }
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = matches!(
            (self.scheme, self.port),
            (Scheme::Http, 80) | (Scheme::Https, 443)
        );
        if default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

fn parse_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(authority) = authority.strip_prefix('[') {
        let close = authority.find(']')?;
        let host = &authority[..close];
        let suffix = &authority[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':')?.parse().ok()?)
        };
        return (!host.is_empty()).then_some((host, port));
    }
    if authority.matches(':').count() > 1 {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().ok()?;
            (!host.is_empty()).then_some((host, Some(port)))
        }
        None => Some((authority, None)),
    }
}

pub(crate) async fn post_json(
    endpoint: &Endpoint,
    body: &str,
    request_timeout: Duration,
    api_key: Option<&str>,
) -> Result<String, AdapterError> {
    timeout(request_timeout, send_request(endpoint, body, api_key))
        .await
        .map_err(|_| AdapterError::Timeout)?
}

async fn send_request(
    endpoint: &Endpoint,
    body: &str,
    api_key: Option<&str>,
) -> Result<String, AdapterError> {
    if api_key.is_some_and(|key| key.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))) {
        return Err(AdapterError::Configuration(
            "API key must not contain a line break".to_string(),
        ));
    }
    let mut stream = connect(endpoint).await?;
    let authorization = api_key
        .map(|key| format!("Authorization: Bearer {key}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
        endpoint.request_target(),
        endpoint.host_header(),
        body.len(),
        authorization,
        body,
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| AdapterError::Transport(format!("HTTP request failed: {error}")))?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.map_err(|error| {
            AdapterError::Transport(format!("failed reading HTTP response: {error}"))
        })?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
        if response_is_complete(&response)? {
            break;
        }
    }
    parse_response(&response)
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

async fn connect(endpoint: &Endpoint) -> Result<Box<dyn AsyncStream>, AdapterError> {
    let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|error| AdapterError::Transport(format!("HTTP connection failed: {error}")))?;
    match endpoint.scheme {
        Scheme::Http => Ok(Box::new(tcp)),
        Scheme::Https => connect_tls(endpoint, tcp).await,
    }
}

#[cfg(feature = "https")]
async fn connect_tls(
    endpoint: &Endpoint,
    tcp: TcpStream,
) -> Result<Box<dyn AsyncStream>, AdapterError> {
    use std::sync::{Arc, OnceLock};

    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG
        .get_or_init(|| {
            let native = rustls_native_certs::load_native_certs();
            if native.certs.is_empty() {
                return Err(format!(
                    "no native TLS root certificates were loaded: {:?}",
                    native.errors
                ));
            }
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(native.certs);
            Ok(Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ))
        })
        .as_ref()
        .map_err(|error| AdapterError::Transport(error.clone()))?;
    let server_name = ServerName::try_from(endpoint.host.clone()).map_err(|error| {
        AdapterError::Configuration(format!(
            "HTTPS endpoint has an invalid server name: {error}"
        ))
    })?;
    let stream = TlsConnector::from(Arc::clone(config))
        .connect(server_name, tcp)
        .await
        .map_err(|error| AdapterError::Transport(format!("TLS connection failed: {error}")))?;
    Ok(Box::new(stream))
}

#[cfg(not(feature = "https"))]
async fn connect_tls(
    _endpoint: &Endpoint,
    _tcp: TcpStream,
) -> Result<Box<dyn AsyncStream>, AdapterError> {
    unreachable!("HTTPS endpoints are rejected while parsing without the https feature")
}

fn response_is_complete(response: &[u8]) -> Result<bool, AdapterError> {
    let Some(header_end) = find_bytes(response, b"\r\n\r\n") else {
        if response.len() > 64 * 1024 {
            return Err(AdapterError::Transport(
                "HTTP response headers exceeded 64 KiB".to_string(),
            ));
        }
        return Ok(false);
    };
    let header = std::str::from_utf8(&response[..header_end]).map_err(|error| {
        AdapterError::Transport(format!("HTTP response headers were not UTF-8: {error}"))
    })?;
    let body = &response[header_end + 4..];
    if is_chunked(header) {
        return decode_chunked(body).map(|decoded| decoded.is_some());
    }
    Ok(content_length(header).is_some_and(|length| body.len() >= length))
}

fn parse_response(response: &[u8]) -> Result<String, AdapterError> {
    let header_end = find_bytes(response, b"\r\n\r\n").ok_or_else(|| {
        AdapterError::Transport("HTTP response did not contain a complete header".to_string())
    })?;
    let header = std::str::from_utf8(&response[..header_end]).map_err(|error| {
        AdapterError::Transport(format!("HTTP response headers were not UTF-8: {error}"))
    })?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| {
            AdapterError::Transport("HTTP response status line was invalid".to_string())
        })?;
    let encoded_body = &response[header_end + 4..];
    let body = if is_chunked(header) {
        decode_chunked(encoded_body)?.ok_or_else(|| {
            AdapterError::Transport("chunked HTTP response was incomplete".to_string())
        })?
    } else if let Some(length) = content_length(header) {
        if encoded_body.len() < length {
            return Err(AdapterError::Transport(
                "HTTP response body was incomplete".to_string(),
            ));
        }
        encoded_body[..length].to_vec()
    } else {
        encoded_body.to_vec()
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    if !(200..300).contains(&status) {
        return Err(AdapterError::Http {
            status,
            body: body.chars().take(400).collect(),
        });
    }
    Ok(body)
}

fn is_chunked(header: &str) -> bool {
    header.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    })
}

fn content_length(header: &str) -> Option<usize> {
    header.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
    })
}

fn decode_chunked(mut body: &[u8]) -> Result<Option<Vec<u8>>, AdapterError> {
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = find_bytes(body, b"\r\n") else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&body[..line_end])
            .map_err(|error| AdapterError::Transport(format!("invalid chunk size: {error}")))?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|error| {
            AdapterError::Transport(format!("invalid chunked HTTP response size: {error}"))
        })?;
        body = &body[line_end + 2..];
        if size == 0 {
            return if body.starts_with(b"\r\n") || find_bytes(body, b"\r\n\r\n").is_some() {
                Ok(Some(decoded))
            } else {
                Ok(None)
            };
        }
        let framed_size = size.checked_add(2).ok_or_else(|| {
            AdapterError::Transport("chunked HTTP response size overflowed".to_string())
        })?;
        if body.len() < framed_size {
            return Ok(None);
        }
        if &body[size..size + 2] != b"\r\n" {
            return Err(AdapterError::Transport(
                "chunked HTTP response had an invalid chunk".to_string(),
            ));
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    #[test]
    fn parses_endpoint_components() {
        let endpoint = Endpoint::parse("http://[::1]:11434/api/generate?debug=1", "ollama")
            .expect("endpoint should parse");
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 11434);
        assert_eq!(endpoint.request_target(), "/api/generate?debug=1");
        assert_eq!(endpoint.host_header(), "[::1]:11434");
    }

    #[test]
    fn decodes_complete_chunked_response() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(response_is_complete(response).unwrap());
        assert_eq!(parse_response(response).unwrap(), "hello");
    }

    #[test]
    fn rejects_non_success_and_clips_body() {
        let body = "x".repeat(500);
        let response = format!(
            "HTTP/1.1 503 Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(
            parse_response(response.as_bytes()),
            Err(AdapterError::Http {
                status: 503,
                body: "x".repeat(400),
            })
        );
    }

    #[test]
    fn rejects_malformed_response() {
        let error = parse_response(b"not HTTP").expect_err("malformed response should fail");
        assert!(error.to_string().contains("complete header"));
    }

    #[tokio::test]
    async fn sends_bearer_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint::parse(
            &format!("http://{}/v1/chat", listener.local_addr().unwrap()),
            "openai",
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 1024];
            let read = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8(request[..read].to_vec()).unwrap()
        });

        let body = post_json(&endpoint, "{}", Duration::from_secs(1), Some("secret"))
            .await
            .unwrap();
        assert_eq!(body, "{}");
        let request = server.await.unwrap();
        assert!(request.contains("Authorization: Bearer secret\r\n"));
    }

    #[tokio::test]
    async fn timeout_covers_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint::parse(
            &format!("http://{}/slow", listener.local_addr().unwrap()),
            "ollama",
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _read = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            sleep(Duration::from_millis(100)).await;
            let _ = stream.write_all(b"{}").await;
        });

        let result = post_json(&endpoint, "{}", Duration::from_millis(10), None).await;
        assert_eq!(result, Err(AdapterError::Timeout));
        server.await.unwrap();
    }

    #[cfg(not(feature = "https"))]
    #[test]
    fn rejects_https_without_feature() {
        let error = Endpoint::parse("https://api.example.com/v1/chat/completions", "openai")
            .expect_err("HTTPS should require its feature");
        assert!(error.to_string().contains("without the 'https' feature"));
    }
}
