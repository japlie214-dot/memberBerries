//! `memb embed-service [run|status|stop]`: the shared resident embedding model (T042).
//!
//! Every memb process on this machine (each host's MCP server, every hook) asks this one
//! process for vectors over a per-user named pipe, so there is one model in memory and no
//! model load per hook. It starts on demand, unloads the model when idle and exits when
//! nothing has asked for a while.
use memb_embed::service::{call, pipe_name, serve, CallError, Request, Response};
use memb_embed::gemma::GemmaEmbedder;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_IDLE_SECS: u64 = 900;
const MODEL_IDLE_SECS: u64 = 300;

fn idle_exit() -> Duration {
    let secs = std::env::var("MEMB_EMBED_SERVICE_IDLE_SECS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|s| *s >= 1);
    Duration::from_secs(secs.unwrap_or(DEFAULT_IDLE_SECS))
}

pub fn run(root: &Path, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("run") if args.len() <= 1 => serve_forever(root),
        Some("status") if args.len() == 1 => status(root),
        Some("stop") if args.len() == 1 => stop(root),
        _ => {
            eprintln!("Usage: memb embed-service [run|status|stop]");
            2
        }
    }
}

fn serve_forever(root: &Path) -> i32 {
    std::env::set_var("MEMB_IN_SERVICE", "1");
    let Some(directory) = std::env::var_os("MEMB_GEMMA_MODEL_DIR").map(std::path::PathBuf::from) else {
        eprintln!("memb: no local embedding model is configured; run `memb models install`");
        return 1;
    };
    let embedder = Arc::new(GemmaEmbedder::new(directory));
    {
        let embedder = embedder.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));
            embedder.unload_if_idle(Duration::from_secs(MODEL_IDLE_SECS));
        });
    }
    let handler = {
        Arc::new(move |request: Request| match request {
            Request::Ping => Response { ok: true, version: Some(env!("CARGO_PKG_VERSION").into()), pid: Some(std::process::id()), ..Response::default() },
            Request::Shutdown => {
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(200));
                    std::process::exit(0);
                });
                Response { ok: true, ..Response::default() }
            }
            Request::Embed { text, query } => match embedder.embed_tagged(&text, query) {
                Ok((tag, vector)) => Response { ok: true, tag: Some(tag), vector: Some(vector), ..Response::default() },
                Err(reason) => Response::failure(reason),
            },
        })
    };
    match serve(&pipe_name(root), handler, idle_exit()) {
        Ok(()) => 0,
        Err(e) => {
            // Another service already owns the pipe: that is fine, there is one.
            eprintln!("memb: embedding service not started: {}", e.kind());
            0
        }
    }
}

fn status(root: &Path) -> i32 {
    match call(&pipe_name(root), &Request::Ping, Duration::from_secs(3)) {
        Ok(r) if r.ok => {
            println!("running: memb {} (pid {})", r.version.unwrap_or_default(), r.pid.unwrap_or(0));
            0
        }
        Ok(r) => {
            println!("unhealthy: {}", r.error.unwrap_or_default());
            1
        }
        Err(CallError::NotRunning) => {
            println!("not running (it starts when a search needs it)");
            0
        }
        Err(e) => {
            println!("unreachable: {e}");
            1
        }
    }
}

fn stop(root: &Path) -> i32 {
    match call(&pipe_name(root), &Request::Shutdown, Duration::from_secs(5)) {
        Ok(_) => {
            println!("stopped");
            0
        }
        Err(CallError::NotRunning) => {
            println!("not running");
            0
        }
        Err(e) => {
            eprintln!("memb: {e}");
            1
        }
    }
}
