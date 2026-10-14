//! Disposable loopback PIR fixture; no wallet data or node connections.
#[path = "../tests/fixture.rs"]
mod fixture;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};
fn request(stream: &mut TcpStream) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == 8192 {
            return Err("header limit".into());
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let header = std::str::from_utf8(&header)?;
    let first = header.lines().next().ok_or("request missing")?.to_owned();
    let lengths: Vec<_> = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .collect::<Result<_, _>>()?;
    if lengths.len() > 1 {
        return Err("duplicate length".into());
    }
    let length = lengths.first().copied().unwrap_or(0);
    if length > 282740 {
        return Err("body limit".into());
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok((first, body))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = fixture::profile();
    let server = fixture::Fixture::new(&profile);
    let material: serde_json::Value = serde_json::from_slice(&server.material)?;
    let session_path = format!(
        "GET /v1/enhance/session/{} HTTP/1.1",
        material["session_id"].as_str().ok_or("missing session")?
    );
    let listener = TcpListener::bind("127.0.0.1:0")?;
    println!(
        "{}",
        serde_json::json!({"port":listener.local_addr()?.port()})
    );
    for stream in listener.incoming() {
        let mut stream = stream?;
        let result = request(&mut stream);
        let (status, content) = match result {
            Ok((path, body)) if path == "GET /metadata HTTP/1.1" && body.is_empty() => (
                200,
                serde_json::to_vec(&serde_json::json!({
                    "manifest":serde_json::from_slice::<serde_json::Value>(&server.manifest)?,
                    "material":serde_json::from_slice::<serde_json::Value>(&server.material)?,
                    "height":3500000,"hash":"ef".repeat(32),"records":67,
                    "expected":([0,32,33,66].into_iter().map(|p|(p.to_string(),hex::encode(fixture::record(p)))).collect::<std::collections::BTreeMap<_,_>>())
                }))?,
            ),
            Ok((path, body)) if path == "GET /v1/enhance/init HTTP/1.1" && body.is_empty() => {
                (200, server.manifest.clone())
            }
            Ok((path, body)) if path == session_path && body.is_empty() => {
                (200, server.material.clone())
            }
            Ok((path, body))
                if (path == "POST /query HTTP/1.1"
                    || path == "POST /v1/enhance/query HTTP/1.1")
                    && body.len() == 110708 =>
            {
                (200, server.answer(&body))
            }
            _ => (400, Vec::new()),
        };
        write!(
            stream,
            "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            content.len()
        )?;
        stream.write_all(&content)?;
    }
    Ok(())
}
