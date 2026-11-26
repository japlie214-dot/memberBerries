//! The shared embedding service: one resident model that every memb process (each host's
//! MCP server, every hook) asks for vectors over a per-user named pipe.
//!
//! Frames are a 4-byte little-endian length followed by JSON. The pipe is local and limited
//! to the current user (see `memb_win::pipe`); there is no network port. This module holds
//! the protocol, the client and a generic server loop; what a request is answered with (the
//! actual model) is supplied by the caller.
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest frame either side accepts. A text and a 1,024-wide vector are far below it.
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Is a service there, and which memb is it?
    Ping,
    /// Embed one text. `query` selects the query or the document prompt of the model.
    Embed { text: String, query: bool },
    /// Ask the service to exit (after answering). `memb embed-service stop` sends it.
    Shutdown,
}

/// The pipe name for one data directory: one service per data directory (and so per user),
/// whatever the path's case or separators.
pub fn pipe_name(data_root: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let key = data_root.to_string_lossy().to_lowercase().replace('/', "\\");
    let hash = Sha256::digest(key.as_bytes());
    format!("memb-embed-v1-{}", hash[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    /// The identity of the model that made the vector (never mixed across tags by clients).
    pub tag: Option<String>,
    pub vector: Option<Vec<f32>>,
    pub version: Option<String>,
    pub pid: Option<u32>,
}

impl Response {
    pub fn failure(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), ..Self::default() }
    }
}

pub fn write_frame(out: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

/// `Ok(None)` is a clean end of the stream between frames.
pub fn read_frame(input: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut head = [0u8; 4];
    match input.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_le_bytes(head) as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// No process owns the pipe: the service is not running.
    NotRunning,
    Failed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::NotRunning => write!(f, "the embedding service is not running"),
            CallError::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

/// One request and its answer, with a deadline. A service that stops answering costs the
/// caller `timeout`, never more (the stuck attempt is abandoned on its own thread).
#[cfg(windows)]
pub fn call(name: &str, request: &Request, timeout: Duration) -> Result<Response, CallError> {
    let payload = serde_json::to_vec(request).map_err(|e| CallError::Failed(e.to_string()))?;
    let name = name.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = (|| {
            let mut pipe = memb_win::pipe::connect(&name).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    CallError::NotRunning
                } else {
                    CallError::Failed(format!("cannot reach the embedding service: {}", e.kind()))
                }
            })?;
            write_frame(&mut pipe, &payload).map_err(|e| CallError::Failed(format!("cannot send to the embedding service: {}", e.kind())))?;
            let body = read_frame(&mut pipe)
                .map_err(|e| CallError::Failed(format!("cannot read the embedding service answer: {}", e.kind())))?
                .ok_or_else(|| CallError::Failed("the embedding service closed the connection".into()))?;
            serde_json::from_slice::<Response>(&body).map_err(|_| CallError::Failed("the embedding service sent something unreadable".into()))
        })();
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(_) => Err(CallError::Failed("the embedding service did not answer in time".into())),
    }
}

/// Answers requests on the named pipe until nothing has asked for `idle` (and no request is
/// running); then it returns, so the caller can exit and give all its memory back.
/// `handler` is called on its own thread for every request.
#[cfg(windows)]
pub fn serve(name: &str, handler: Arc<dyn Fn(Request) -> Response + Send + Sync>, idle: Duration) -> std::io::Result<()> {
    let mut listener = memb_win::pipe::PipeListener::bind(name)?;
    let last = Arc::new(Mutex::new(Instant::now()));
    let active = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (last, active, stop, name) = (last.clone(), active.clone(), stop.clone(), name.to_owned());
        std::thread::spawn(move || {
            let tick = (idle / 4).clamp(Duration::from_millis(20), Duration::from_secs(1));
            loop {
                std::thread::sleep(tick);
                let quiet = last.lock().map(|l| l.elapsed() >= idle).unwrap_or(true);
                if quiet && active.load(Ordering::SeqCst) == 0 {
                    stop.store(true, Ordering::SeqCst);
                    let _ = memb_win::pipe::connect(&name); // wakes the blocking accept
                    return;
                }
            }
        });
    }
    loop {
        let mut stream = listener.accept()?;
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Ok(mut l) = last.lock() {
            *l = Instant::now();
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (handler, active, last) = (handler.clone(), active.clone(), last.clone());
        std::thread::spawn(move || {
            while let Ok(Some(body)) = read_frame(&mut stream) {
                let response = match serde_json::from_slice::<Request>(&body) {
                    Ok(request) => handler(request),
                    Err(_) => Response::failure("unreadable request"),
                };
                let sent = serde_json::to_vec(&response).map(|b| write_frame(&mut stream, &b));
                if !matches!(sent, Ok(Ok(()))) {
                    break;
                }
            }
            if let Ok(mut l) = last.lock() {
                *l = Instant::now();
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn unique(tag: &str) -> String {
        format!("memb-svc-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    /// A model stand-in: the vector is the text's bytes as floats, the tag names the prompt kind.
    fn fake(request: Request) -> Response {
        match request {
            Request::Ping => Response { ok: true, version: Some("test".into()), pid: Some(std::process::id()), ..Response::default() },
            Request::Shutdown => Response { ok: true, ..Response::default() },
            Request::Embed { text, query } => {
                if text == "boom" {
                    return Response::failure("the model failed");
                }
                if text == "slow" {
                    std::thread::sleep(Duration::from_millis(600));
                }
                Response { ok: true, tag: Some(if query { "fake|query" } else { "fake|doc" }.into()), vector: Some(text.bytes().map(f32::from).collect()), ..Response::default() }
            }
        }
    }

    #[test]
    fn one_data_directory_has_one_stable_pipe_name() {
        let a = pipe_name(std::path::Path::new(r"C:\Users\Me\AppData\Local\memberBerries"));
        assert_eq!(a, pipe_name(std::path::Path::new("c:/users/me/appdata/local/memberberries")), "case and separators do not matter");
        assert_ne!(a, pipe_name(std::path::Path::new(r"C:\Users\Other\AppData\Local\memberBerries")));
        assert!(a.starts_with("memb-embed-v1-") && a.len() == "memb-embed-v1-".len() + 16);
        assert!(memb_win::pipe::pipe_path(&a).is_ok(), "a valid pipe name");
    }

    #[test]
    fn frames_round_trip_and_oversize_or_truncated_frames_are_refused() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"hello").unwrap();
        assert_eq!(read_frame(&mut buffer.as_slice()).unwrap().unwrap(), b"hello");
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none(), "end of stream between frames is clean");
        assert!(read_frame(&mut [5u8, 0, 0, 0, 1].as_slice()).is_err(), "a frame cut short is an error");
        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut huge.as_slice()).is_err());
        assert!(write_frame(&mut Vec::new(), &vec![0u8; MAX_FRAME + 1]).is_err());
    }

    #[test]
    fn a_service_answers_pings_and_embeddings_errors_and_many_clients_at_once() {
        let name = unique("answers");
        assert_eq!(call(&name, &Request::Ping, Duration::from_secs(1)), Err(CallError::NotRunning));
        let server = {
            let name = name.clone();
            std::thread::spawn(move || serve(&name, Arc::new(fake), Duration::from_millis(800)))
        };
        // The pipe appears when the server has bound it.
        let mut pong = None;
        for _ in 0..100 {
            if let Ok(r) = call(&name, &Request::Ping, Duration::from_secs(2)) {
                pong = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pong = pong.expect("the service came up");
        assert!(pong.ok && pong.version.as_deref() == Some("test"));
        let r = call(&name, &Request::Embed { text: "ab".into(), query: true }, Duration::from_secs(2)).unwrap();
        assert_eq!((r.tag.as_deref(), r.vector), (Some("fake|query"), Some(vec![97.0, 98.0])));
        let r = call(&name, &Request::Embed { text: "ab".into(), query: false }, Duration::from_secs(2)).unwrap();
        assert_eq!(r.tag.as_deref(), Some("fake|doc"));
        let r = call(&name, &Request::Embed { text: "boom".into(), query: false }, Duration::from_secs(2)).unwrap();
        assert!(!r.ok && r.error.as_deref() == Some("the model failed"));
        // Several clients at the same time, each with its own text, each gets its own answer.
        let handles: Vec<_> = (0..6u8)
            .map(|i| {
                let name = name.clone();
                std::thread::spawn(move || {
                    let text = format!("t{i}");
                    let r = call(&name, &Request::Embed { text: text.clone(), query: true }, Duration::from_secs(3)).unwrap();
                    assert_eq!(r.vector.unwrap(), text.bytes().map(f32::from).collect::<Vec<_>>());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // A request that takes longer than the caller allows fails for the caller only.
        let started = Instant::now();
        let r = call(&name, &Request::Embed { text: "slow".into(), query: true }, Duration::from_millis(100));
        assert!(matches!(r, Err(CallError::Failed(m)) if m.contains("in time")));
        assert!(started.elapsed() < Duration::from_millis(500));
        // Idle: with no more requests the server returns by itself and the pipe disappears.
        server.join().unwrap().unwrap();
        assert_eq!(call(&name, &Request::Ping, Duration::from_secs(1)), Err(CallError::NotRunning));
    }

    #[test]
    fn a_service_with_no_clients_stops_after_the_idle_time_and_one_name_has_one_owner() {
        let name = unique("idle");
        let first = {
            let name = name.clone();
            std::thread::spawn(move || serve(&name, Arc::new(fake), Duration::from_millis(300)))
        };
        std::thread::sleep(Duration::from_millis(100));
        // A second service for the same name cannot start: the first owns the pipe.
        assert!(serve(&name, Arc::new(fake), Duration::from_millis(300)).is_err());
        let started = Instant::now();
        first.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "stopped after the idle time");
    }
}
