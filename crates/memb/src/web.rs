//! Read-only, loopback-only HTTP transport for the shared inspector.

use super::viewer;
use memb_core::Scope;
use memb_store::{Error, Store};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    time::{Duration, Instant},
};

const MAX_HEADERS: usize = 8 * 1024;
const SOCKET_BUDGET: Duration = Duration::from_secs(2);
const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json";
const TEXT: &str = "text/plain; charset=utf-8";

pub fn serve(store: Store, pinned: Option<Scope>, port: u16) -> Result<(), Error> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let access_key = page_nonce()?;
    {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{origin}/#{access_key}")?;
        stdout.flush()?;
    }

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Client disconnects and timeouts must not stop the listener.
                let _ = handle(
                    &mut stream,
                    &host,
                    &origin,
                    &store,
                    pinned.as_ref(),
                    &access_key,
                );
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        }
    }
}

type Status = &'static str;

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(left)
    }
}

fn read_headers(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, Status> {
    let deadline = Instant::now() + SOCKET_BUDGET;
    let mut used = 0;
    while used < buf.len() {
        let timeout = remaining(deadline).map_err(|_| "408 Request Timeout")?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| "400 Bad Request")?;
        match stream.read(&mut buf[used..]) {
            Ok(0) => return Err("400 Bad Request"),
            Ok(n) => {
                // Only the last three previous bytes could start a new terminator.
                if let Some(end) = buf[used.saturating_sub(3)..used + n]
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                {
                    return Ok(used.saturating_sub(3) + end);
                }
                used += n;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("408 Request Timeout");
            }
            Err(_) => return Err("400 Bad Request"),
        }
    }
    Err("431 Request Header Fields Too Large")
}

struct Request<'a> {
    method: &'a str,
    target: &'a str,
    headers: BTreeMap<String, &'a str>,
}

impl<'a> Request<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut lines = std::str::from_utf8(bytes).ok()?.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?;
        let target = request_line.next()?;
        let version = request_line.next()?;
        if !token(method)
            || !target.starts_with('/')
            || target.starts_with("//")
            || !target.bytes().all(|b| b.is_ascii_graphic() && b != b'#')
            || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || request_line.next().is_some()
        {
            return None;
        }
        let mut headers = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once(':')?;
            if !token(name)
                || value.bytes().any(|b| b != b'\t' && (b < b' ' || b == 127))
                || headers
                    .insert(name.to_ascii_lowercase(), value.trim_matches([' ', '\t']))
                    .is_some()
            {
                return None;
            }
        }
        Some(Self {
            method,
            target,
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).copied()
    }

    fn authorize(&self, host: &str, origin: &str) -> Result<(), Status> {
        if self.header("host") != Some(host)
            || self.header("origin").is_some_and(|value| value != origin)
            || !matches!(
                self.header("sec-fetch-site"),
                None | Some("same-origin" | "none")
            )
        {
            return Err("403 Forbidden");
        }
        if self.method != "GET" {
            return Err("405 Method Not Allowed");
        }
        if self.header("transfer-encoding").is_some()
            || !matches!(self.header("content-length"), None | Some("0"))
            || self.header("expect").is_some()
        {
            return Err("400 Bad Request");
        }
        Ok(())
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn args(query: &str) -> Result<Value, &'static str> {
    let mut args = Map::new();
    if !query.is_empty() {
        for field in query.split('&') {
            let (name, value) = field.split_once('=').ok_or("invalid query parameter")?;
            let name = decode(name)?;
            if !matches!(name.as_str(), "project" | "after" | "query") {
                return Err("unexpected query parameter");
            }
            if args.insert(name, Value::String(decode(value)?)).is_some() {
                return Err("duplicate query parameter");
            }
        }
    }
    Ok(Value::Object(args))
}

fn decode(value: &str) -> Result<String, &'static str> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let mut bytes = value.bytes();
    let mut decoded = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let high = bytes.next().and_then(hex).ok_or("invalid URL encoding")?;
                let low = bytes.next().and_then(hex).ok_or("invalid URL encoding")?;
                (high << 4) | low
            }
            _ => byte,
        });
    }
    String::from_utf8(decoded).map_err(|_| "invalid UTF-8 in query parameter")
}

fn handle(
    stream: &mut TcpStream,
    host: &str,
    origin: &str,
    store: &Store,
    pinned: Option<&Scope>,
    access_key: &str,
) -> io::Result<()> {
    let mut buf = [0; MAX_HEADERS];
    let request = read_headers(stream, &mut buf).and_then(|end| {
        let request = Request::parse(&buf[..end]).ok_or("400 Bad Request")?;
        request.authorize(host, origin)?;
        Ok(request)
    });
    let request = match request {
        Ok(request) => request,
        Err(status) => return reject(stream, status),
    };
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((request.target, ""));
    match path {
        "/" => {
            let nonce = match page_nonce() {
                Ok(nonce) => nonce,
                Err(_) => return reject(stream, "500 Internal Server Error"),
            };
            // Only tag the trusted, static inline blocks from the shared HTML.
            let page = viewer::HTML
                .replace("<script>", &format!("<script nonce=\"{nonce}\">"))
                .replace("<style>", &format!("<style nonce=\"{nonce}\">"));
            let policy = format!(
                "; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; connect-src 'self'"
            );
            respond(stream, "200 OK", HTML, page.as_bytes(), &policy)
        }
        "/api/snapshot" | "/api/projects" | "/api/graph" => {
            if request.header("x-memb-viewer") != Some(access_key) {
                return reject(stream, "403 Forbidden");
            }
            let (status, value) = match args(query).and_then(|args| match path {
                "/api/projects" => viewer::projects(store, pinned, &args),
                "/api/graph" => viewer::graph(store, pinned, &args),
                _ => viewer::snapshot(store, pinned, &args),
            }) {
                Ok(value) => ("200 OK", value),
                Err(message) => ("400 Bad Request", json!({"error":message})),
            };
            respond(stream, status, JSON, value.to_string().as_bytes(), "")
        }
        _ => reject(stream, "404 Not Found"),
    }
}

fn reject(stream: &mut TcpStream, status: Status) -> io::Result<()> {
    respond(stream, status, TEXT, status.as_bytes(), "")
}

fn respond(
    stream: &mut TcpStream,
    status: Status,
    content_type: &str,
    body: &[u8],
    policy: &str,
) -> io::Result<()> {
    let allow = if status == "405 Method Not Allowed" {
        "Allow: GET\r\n"
    } else {
        ""
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\n{allow}\
         Content-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\n\
         Content-Security-Policy: default-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'{policy}\r\n\r\n",
        body.len(),
    );
    let deadline = Instant::now() + SOCKET_BUDGET;
    for mut part in [headers.as_bytes(), body] {
        while !part.is_empty() {
            stream.set_write_timeout(Some(remaining(deadline)?))?;
            match stream.write(part) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => part = &part[n..],
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

fn page_nonce() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn random_bytes(bytes: &mut [u8; 16]) -> io::Result<()> {
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}

#[cfg(windows)]
fn random_bytes(bytes: &mut [u8; 16]) -> io::Result<()> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut (), buffer: *mut u8, len: u32, flags: u32) -> i32;
    }
    const USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    // SAFETY: The system RNG accepts a null algorithm handle with this flag;
    // the output pointer is valid for the supplied length.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        Err(io::Error::other("system random generator failed"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "127.0.0.1:45123";
    const ORIGIN: &str = "http://127.0.0.1:45123";

    #[test]
    fn serves_html_and_protects_snapshots_over_loopback() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let host = address.to_string();
        let origin = format!("http://{host}");
        let store = Store::open(&std::env::temp_dir().join(format!(
            "memb-web-test-{}-{}",
            std::process::id(),
            address.port()
        )))
        .unwrap();
        let scope = Scope::parse("example").unwrap();
        let exchange = |target: &str, extra_headers: &str| {
            let mut client = TcpStream::connect(address).unwrap();
            client.set_read_timeout(Some(SOCKET_BUDGET)).unwrap();
            write!(
                client,
                "GET {target} HTTP/1.1\r\nHost: {host}\r\n{extra_headers}\r\n"
            )
            .unwrap();
            let (mut server, _) = listener.accept().unwrap();
            handle(&mut server, &host, &origin, &store, Some(&scope), "1").unwrap();
            drop(server);
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            assert!(response.contains("\r\nCache-Control: no-store\r\n"));
            assert!(!response.contains("\r\nAccess-Control-Allow-"));
            response
        };

        let page = exchange("/", "");
        let (headers, html) = page.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        let nonce = headers
            .split_once("script-src 'nonce-")
            .unwrap()
            .1
            .split('\'')
            .next()
            .unwrap();
        assert!(html.contains(&format!("<script nonce=\"{nonce}\">")));
        assert!(html.contains(&format!("<style nonce=\"{nonce}\">")));

        for path in ["/api/snapshot", "/api/projects", "/api/graph"] {
            assert!(exchange(path, "").starts_with("HTTP/1.1 403 Forbidden\r\n"));
        }
        let projects = exchange("/api/projects", "X-Memb-Viewer: 1\r\n");
        let data: Value = serde_json::from_str(projects.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(data["projects"], json!(["example"]));
        let graph = exchange("/api/graph?project=example", "X-Memb-Viewer: 1\r\n");
        let data: Value = serde_json::from_str(graph.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(data["notes"], json!([]));
        assert!(exchange("/api/graph?project=wrong", "X-Memb-Viewer: 1\r\n")
            .starts_with("HTTP/1.1 400 Bad Request\r\n"));
        let snapshot = exchange("/api/snapshot?after=0", "X-Memb-Viewer: 1\r\n");
        assert!(snapshot.starts_with("HTTP/1.1 200 OK\r\n"));
        let data: Value = serde_json::from_str(snapshot.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(data["project"], "example");
        assert!(
            exchange("/api/snapshot?project=wrong", "X-Memb-Viewer: 1\r\n")
                .starts_with("HTTP/1.1 400 Bad Request\r\n")
        );
    }

    #[test]
    fn preserves_query_parameters_as_text() {
        assert_eq!(
            args("project=example&after=18446744073709551615&query=a%2Bb+%C3%A9").unwrap(),
            json!({"project":"example", "after":"18446744073709551615", "query":"a+b é"})
        );
        assert_eq!(args("").unwrap(), json!({}));
        for query in [
            "after=1&%61fter=2",
            "query=%FF",
            "query=%A",
            "other=x",
            "project",
        ] {
            assert!(args(query).is_err(), "{query}");
        }
    }

    #[test]
    fn authorizes_only_exact_local_requests_and_get() {
        fn check(method: &str, headers: &str) -> Result<(), Status> {
            let request = format!("{method} / HTTP/1.1\r\n{headers}");
            Request::parse(request.as_bytes())
                .ok_or("400 Bad Request")?
                .authorize(HOST, ORIGIN)
        }
        assert_eq!(
            check("GET", "hOsT: 127.0.0.1:45123\r\nSec-Fetch-Site: none"),
            Ok(())
        );
        for headers in [
            "Host: localhost:45123",
            "Host: 127.0.0.1:45123\r\nOrigin: null",
            "Host: 127.0.0.1:45123\r\nOrigin: http://127.0.0.1:45124",
            "Host: 127.0.0.1:45123\r\nSec-Fetch-Site: same-site",
            "Host: 127.0.0.1:45123\r\nSec-Fetch-Site: cross-site",
        ] {
            assert_eq!(check("GET", headers), Err("403 Forbidden"));
        }
        assert_eq!(
            check("GET", "Host: 127.0.0.1:45123\r\nHOST: other"),
            Err("400 Bad Request")
        );
        assert_eq!(
            check("GET", "Host: 127.0.0.1:45123\r\nContent-Length: 1"),
            Err("400 Bad Request")
        );
        for method in ["HEAD", "POST", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(
                check(method, "Host: 127.0.0.1:45123"),
                Err("405 Method Not Allowed")
            );
        }
    }
}
