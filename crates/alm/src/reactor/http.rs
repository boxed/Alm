//! Just enough HTTP/1.1 to serve a project on localhost.
//!
//! elm's reactor runs on Snap. alm has no dependencies, so this is the small
//! amount of protocol a dev server actually needs: read a request line and its
//! headers, answer with a length-delimited response, close. No keep-alive, no
//! chunked encoding, no compression — all of which only matter across a
//! network, and this never leaves the loopback interface.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

pub struct Request {
    pub method: String,
    /// The path with its query string stripped and `%XX` decoded.
    pub path: String,
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: String,
    pub body: Body,
}

pub enum Body {
    Bytes(Vec<u8>),
    /// A file to stream, so serving a large asset does not read it all in.
    File(std::path::PathBuf, u64),
}

impl Response {
    pub fn html(body: String) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "text/html;charset=utf-8".to_string(),
            body: Body::Bytes(body.into_bytes()),
        }
    }

    pub fn not_found(body: String) -> Response {
        Response { status: 404, reason: "Not Found", ..Response::html(body) }
    }

    pub fn file(path: &Path, content_type: &str, length: u64) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: content_type.to_string(),
            body: Body::File(path.to_path_buf(), length),
        }
    }
}

/// Serve until the process is killed. `handle` runs on its own thread per
/// connection: a dev server answers one browser, and a compile takes long
/// enough that a single-threaded loop would stall the page's other requests.
pub fn serve<H>(listener: TcpListener, handle: H) -> !
where
    H: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handle = std::sync::Arc::new(handle);
    loop {
        let Ok((stream, _)) = listener.accept() else { continue };
        let handle = handle.clone();
        std::thread::spawn(move || {
            // A browser that goes away mid-request is ordinary, not an error.
            let _ = respond(stream, handle.as_ref());
        });
    }
}

fn respond<H>(mut stream: TcpStream, handle: &H) -> std::io::Result<()>
where
    H: Fn(&Request) -> Response,
{
    let Some(request) = read_request(&stream)? else {
        return Ok(());
    };
    let response = match request.method.as_str() {
        "GET" | "HEAD" => handle(&request),
        _ => Response {
            status: 405,
            reason: "Method Not Allowed",
            content_type: "text/plain;charset=utf-8".to_string(),
            body: Body::Bytes(b"Only GET is supported.".to_vec()),
        },
    };

    let length = match &response.body {
        Body::Bytes(bytes) => bytes.len() as u64,
        Body::File(_, length) => *length,
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {length}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        response.status, response.reason, response.content_type
    );
    stream.write_all(head.as_bytes())?;
    if request.method == "HEAD" {
        return stream.flush();
    }
    match response.body {
        Body::Bytes(bytes) => stream.write_all(&bytes)?,
        Body::File(path, _) => {
            let mut file = std::fs::File::open(path)?;
            std::io::copy(&mut file, &mut stream)?;
        }
    }
    stream.flush()
}

/// Read the request line and drain the headers. `None` for an empty or
/// malformed request, which browsers do open (speculative connections).
fn read_request(stream: &TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    // Headers are not used, but they have to come off the socket before the
    // response goes out or a client can see a truncated exchange.
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim_end().is_empty() {
            break;
        }
    }
    let target = target.split(['?', '#']).next().unwrap_or("");
    Ok(Some(Request { method: method.to_string(), path: percent_decode(target) }))
}

/// `%20` and friends. An invalid escape is left alone rather than dropped: a
/// file really can be named `100%`.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape text for HTML body or attribute content.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode the characters that would change how a URL path parses.
pub fn encode_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Read a whole file, or `None` if it cannot be read.
pub fn read(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_escapes_are_decoded_and_bad_ones_left_alone() {
        assert_eq!(percent_decode("/My%20Files/a.elm"), "/My Files/a.elm");
        assert_eq!(percent_decode("/100%"), "/100%");
        assert_eq!(percent_decode("/%zz"), "/%zz");
        assert_eq!(percent_decode("/caf%C3%A9"), "/café");
    }

    #[test]
    fn paths_round_trip_through_encoding() {
        assert_eq!(encode_path("/My Files/a.elm"), "/My%20Files/a.elm");
        assert_eq!(percent_decode(&encode_path("/a b/c?d")), "/a b/c?d");
    }

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(escape("<a href=\"x\">&</a>"), "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;");
    }
}
