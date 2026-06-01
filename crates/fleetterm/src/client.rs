//! Bridge between the `fleetd` unix socket (tokio) and the GPUI UI (smol executor).
//!
//! A dedicated OS thread runs a current-thread tokio runtime that connects to the daemon,
//! subscribes, and pumps two `async-channel`s:
//!   * `events`  — daemon [`Event`]s out to the UI (the UI awaits these via `cx.spawn`).
//!   * `requests`— UI [`Request`]s in to the daemon.
//!
//! `async-channel` (not `tokio::sync::mpsc`) is used deliberately: its futures are
//! runtime-agnostic, so the GPUI/smol side can await `events` without a tokio reactor.
//!
//! This module is intentionally gpui-free so it compiles + tests independently of the
//! renderer (and survives a UI-framework pivot).

use std::path::PathBuf;

use async_channel::{Receiver, Sender};
use protocol::codec::{self, MAX_FRAME};
use protocol::{Event, Frame, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub struct FleetClient {
    /// Daemon events for the UI to render.
    pub events: Receiver<Event>,
    /// UI → daemon requests.
    pub requests: Sender<Request>,
}

impl FleetClient {
    /// Connect to the daemon on `sock_path` (defaults to the standard runtime path),
    /// spawning the background tokio thread. Returns immediately; connection happens on
    /// the thread and surfaces as events (or a logged error).
    pub fn connect(sock_path: Option<PathBuf>) -> FleetClient {
        let sock = sock_path.unwrap_or_else(default_sock_path);
        let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
        let (req_tx, req_rx) = async_channel::unbounded::<Request>();

        std::thread::Builder::new()
            .name("fleetd-client".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("client runtime: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = run(&sock, ev_tx.clone(), req_rx).await {
                        tracing::warn!("fleetd client ended: {e}");
                        let _ = ev_tx
                            .send(Event::Error {
                                message: format!("daemon connection lost: {e}"),
                            })
                            .await;
                    }
                });
            })
            .expect("spawn fleetd-client thread");

        FleetClient {
            events: ev_rx,
            requests: req_tx,
        }
    }
}

async fn run(
    sock: &PathBuf,
    ev_tx: Sender<Event>,
    req_rx: Receiver<Request>,
) -> anyhow::Result<()> {
    let stream = UnixStream::connect(sock).await?;
    let (mut rd, mut wr) = stream.into_split();

    // Subscribe immediately.
    write_frame(&mut wr, &Frame::Request(Request::Subscribe)).await?;

    // Writer task: drain UI requests to the socket.
    let writer = tokio::spawn(async move {
        while let Ok(req) = req_rx.recv().await {
            if write_frame(&mut wr, &Frame::Request(req)).await.is_err() {
                break;
            }
        }
    });

    // Reader loop: daemon events to the UI.
    loop {
        let ev: Event = match read_frame(&mut rd).await {
            Ok(ev) => ev,
            Err(_) => break,
        };
        if ev_tx.send(ev).await.is_err() {
            break; // UI dropped
        }
    }
    writer.abort();
    Ok(())
}

// --- async length-prefixed msgpack framing (same wire format as protocol::codec) ---

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> anyhow::Result<()> {
    let buf = codec::encode(frame)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Event> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len);
    if n > MAX_FRAME {
        anyhow::bail!("frame too large: {n}");
    }
    let mut body = vec![0u8; n as usize];
    r.read_exact(&mut body).await?;
    Ok(rmp_serde::from_slice(&body)?)
}

fn default_sock_path() -> PathBuf {
    std::env::var("FLEETTERM_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("fleetterm")
                .join("fleetd.sock")
        })
}
