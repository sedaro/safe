use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Sends the subset of HTTP needed by the local Ollama integration.
pub async fn post_json(host: &str, port: u16, path: &str, body: &str) -> Result<Response> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| anyhow!("HTTP connection failed: {e}"))?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let host_header = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| anyhow!("HTTP request failed: {e}"))?;
    stream
        .shutdown()
        .await
        .map_err(|e| anyhow!("HTTP request shutdown failed: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| anyhow!("failed reading HTTP response: {e}"))?;
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Result<Response> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("HTTP response did not contain a complete header"))?;
    let header = std::str::from_utf8(&response[..header_end])?;
    let mut lines = header.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| anyhow!("HTTP response did not contain a status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("HTTP response status line was invalid"))?
        .parse::<u16>()?;
    let chunked = lines.any(|line| {
        line.split_once(':')
            .map(|(name, value)| {
                name.eq_ignore_ascii_case("transfer-encoding")
                    && value
                        .split(',')
                        .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
            })
            .unwrap_or(false)
    });
    let body = &response[header_end + 4..];
    let body = if chunked {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };

    Ok(Response {
        status,
        body: String::from_utf8(body)?,
    })
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| anyhow!("chunked HTTP response had an invalid size line"))?;
        let size_text = std::str::from_utf8(&body[..line_end])?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let size = usize::from_str_radix(size_text, 16)?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        if body.len() < size + 2 || &body[size..size + 2] != b"\r\n" {
            bail!("chunked HTTP response had an invalid chunk");
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    #[test]
    fn parses_plain_response() {
        let response = parse_response(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello").unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
    }

    #[test]
    fn decodes_chunked_response() {
        let response = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.body, "hello");
    }
}
