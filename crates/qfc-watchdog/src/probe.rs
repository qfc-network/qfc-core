//! Plain-std HTTP probes — no async runtime, no HTTP client dependency.
//!
//! Requests are sent as `HTTP/1.0` + `Connection: close`, which forbids
//! chunked transfer encoding, so "read to EOF, split on the blank line" is a
//! correct response parser for both tiny_http (node metrics) and hyper
//! (jsonrpsee RPC).

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn http_exchange(addr: SocketAddr, request: &str, timeout: Duration) -> Result<String> {
    let stream =
        TcpStream::connect_timeout(&addr, timeout).with_context(|| format!("connect to {addr}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut stream = stream;
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("write request to {addr}"))?;
    let mut buf = String::new();
    stream
        .read_to_string(&mut buf)
        .with_context(|| format!("read response from {addr}"))?;
    let (head, body) = buf
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from {addr}"))?;
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200") {
        return Err(anyhow!("non-200 response from {addr}: {status}"));
    }
    Ok(body.to_string())
}

/// Scrape the node's Prometheus endpoint; returns the raw exposition text.
pub fn scrape_metrics(addr: SocketAddr, timeout: Duration) -> Result<String> {
    let request = format!("GET /metrics HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    http_exchange(addr, &request, timeout)
}

/// Probe the node's JSON-RPC port with `eth_blockNumber`; returns the height.
pub fn rpc_probe(addr: SocketAddr, timeout: Duration) -> Result<u64> {
    let payload = r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#;
    let request = format!(
        "POST / HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    let body = http_exchange(addr, &request, timeout)?;
    parse_block_number(&body)
}

fn parse_block_number(body: &str) -> Result<u64> {
    const MARKER: &str = r#""result":"0x"#;
    let idx = body
        .find(MARKER)
        .ok_or_else(|| anyhow!("no hex result in RPC response: {body}"))?;
    let hex = &body[idx + MARKER.len()..];
    let end = hex
        .find('"')
        .ok_or_else(|| anyhow!("malformed RPC result: {body}"))?;
    u64::from_str_radix(&hex[..end], 16).context("parse block number hex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eth_block_number_response() {
        let body = r#"{"jsonrpc":"2.0","result":"0x1a4","id":1}"#;
        assert_eq!(parse_block_number(body).unwrap(), 0x1a4);
    }

    #[test]
    fn rejects_error_response() {
        let body = r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"nope"},"id":1}"#;
        assert!(parse_block_number(body).is_err());
    }
}
