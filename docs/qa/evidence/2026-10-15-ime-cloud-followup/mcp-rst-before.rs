use agent_mcp::Server;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn send(server: &Server, token: &str, extra: &str, large: bool) -> TcpStream {
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let pad = if large { "x".repeat(60_000) } else { String::new() };
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-11-25\"}},\"pad\":\"{pad}\"}}");
    let wire = format!("POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{}", server.addr, token, body.len(), extra, body);
    stream.write_all(wire.as_bytes()).unwrap();
    stream
}

fn check(bytes: &[u8]) -> Option<(u16, bool, usize, usize)> {
    let split = bytes.windows(4).position(|v| v == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..split]).unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let len: usize = head.lines().find_map(|line| line.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
    let got = bytes.len() - split - 4;
    Some((status, got == len, got, len))
}

fn framed(stream: TcpStream, expected: u16) -> std::io::Result<bool> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Ok(false); }
        head.push_str(&line);
        if line == "\r\n" { break; }
    }
    let len: usize = head.lines().find_map(|line| line.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
    assert!(len <= 4096);
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&body);
    let (status, full, _, _) = check(&bytes).unwrap();
    assert_eq!(status, expected);
    Ok(full)
}

fn main() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    for (name, credential, extra, expected) in [
        ("invalid_token", "bad", "", 401),
        ("invalid_origin", token.as_str(), "Origin: https://evil.example\r\n", 403),
        ("duplicate_auth", token.as_str(), "Authorization: Bearer other\r\n", 401),
        ("invalid_protocol", token.as_str(), "MCP-Protocol-Version: invalid\r\n", 400),
        ("valid_initialize", token.as_str(), "", 200),
    ] {
        for large in [false, true] {
            let mut resets = 0;
            let mut complete = 0;
            let mut errors = 0;
            let mut missing_head = 0;
            let mut truncated = 0;
            for _ in 0..20 {
                let mut stream = send(&server, credential, extra, large);
                let mut bytes = Vec::new();
                if let Err(error) = stream.read_to_end(&mut bytes) {
                    errors += 1;
                    if error.kind() == std::io::ErrorKind::ConnectionReset { resets += 1; }
                }
                if let Some((status, full, _, _)) = check(&bytes) {
                    assert_eq!(status, expected);
                    complete += usize::from(full);
                    truncated += usize::from(!full);
                } else {
                    missing_head += 1;
                }
            }
            println!("{{\"read\":\"to_eof\",\"case\":\"{name}\",\"large\":{large},\"attempts\":20,\"complete_content_length\":{complete},\"missing_head\":{missing_head},\"truncated\":{truncated},\"connection_reset\":{resets},\"errors\":{errors}}}");
            let mut framed_complete = 0;
            let mut framed_errors = 0;
            let mut framed_short = 0;
            for _ in 0..20 {
                let stream = send(&server, credential, extra, large);
                match framed(stream, expected) {
                    Ok(true) => framed_complete += 1,
                    Ok(false) => framed_short += 1,
                    Err(_) => framed_errors += 1,
                }
            }
            println!("{{\"read\":\"content_length\",\"case\":\"{name}\",\"large\":{large},\"attempts\":20,\"complete_response\":{framed_complete},\"short\":{framed_short},\"errors\":{framed_errors}}}");
        }
    }
}
