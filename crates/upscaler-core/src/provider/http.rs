//! Shared bits for the HTTP image services.

use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use image::RgbImage;
use reqwest::blocking::Client;
use serde_json::Value;

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("ai-video-upscaler/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()?)
}

pub fn png_bytes(img: &RgbImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)?;
    Ok(buf)
}

pub fn png_base64(img: &RgbImage) -> Result<String> {
    Ok(base64::engine::general_purpose::STANDARD.encode(png_bytes(img)?))
}

pub fn png_data_uri(img: &RgbImage) -> Result<String> {
    Ok(format!("data:image/png;base64,{}", png_base64(img)?))
}

pub fn decode_base64_image(b64: &str) -> Result<RgbImage> {
    let b64 = b64.split_once("base64,").map(|(_, d)| d).unwrap_or(b64);
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
    super::decode(&bytes)
}

pub fn download(client: &Client, url: &str) -> Result<RgbImage> {
    let bytes = client
        .get(url)
        .send()?
        .error_for_status()?
        .bytes()
        .context("download of the result")?;
    super::decode(&bytes)
}

/// The most useful error text in a service's JSON answer.
pub fn error_text(body: &Value) -> String {
    let e = &body["error"];
    e["message"]
        .as_str()
        .or_else(|| e.as_str())
        .or_else(|| body["detail"].as_str())
        .or_else(|| body["detail"][0]["msg"].as_str())
        .or_else(|| body["message"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| body.to_string().chars().take(400).collect())
}

/// An API key from the settings, else from the first of `env` that is set.
pub fn key(setting: &str, env: &[&str]) -> String {
    let s = setting.trim();
    if !s.is_empty() {
        return s.to_string();
    }
    env.iter()
        .find_map(|v| std::env::var(v).ok().filter(|k| !k.trim().is_empty()))
        .unwrap_or_default()
}

/// A tiny HTTP server for tests: answers requests in order with `responses` (status, body,
/// content type) and returns what it received (request line, headers and body).
#[cfg(test)]
pub mod mock {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    pub struct Received {
        pub head: String,
        pub body: Vec<u8>,
    }

    pub fn serve(
        responses: Vec<(u16, Vec<u8>, &'static str)>,
    ) -> (String, mpsc::Receiver<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body, ctype) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                }
                let mut req_body = vec![0u8; len];
                reader.read_exact(&mut req_body).unwrap();
                tx.send(Received {
                    head,
                    body: req_body,
                })
                .unwrap();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (base, rx)
    }

    pub fn json(status: u16, v: serde_json::Value) -> (u16, Vec<u8>, &'static str) {
        (status, v.to_string().into_bytes(), "application/json")
    }
}
