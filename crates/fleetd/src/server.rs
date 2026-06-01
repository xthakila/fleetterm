//! The unix-socket server. Accepts both UI clients and hook forwarders on one listener,
//! dispatching by [`Frame`]. UI connections are full-duplex: a reader loop handles
//! requests while a spawned task forwards broadcast [`Event`]s back.

use std::sync::Arc;

use anyhow::Result;
use protocol::{Frame, HookDecision, Request, State};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex as AsyncMutex;

use crate::daemon::Daemon;
use crate::{framed, hooks};

pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let path = daemon.sock_path.clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    tracing::info!("fleetd listening on {}", path.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let d = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(d, stream).await {
                tracing::debug!("connection ended: {e}");
            }
        });
    }
}

async fn handle_conn(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (mut rd, wr) = stream.into_split();
    let wr = Arc::new(AsyncMutex::new(wr));

    loop {
        let frame: Frame = match framed::read_frame(&mut rd).await {
            Ok(f) => f,
            Err(_) => break, // clean close or error → drop the connection
        };

        match frame {
            Frame::Hook(env) => {
                let reply = hooks::handle_hook(&daemon.reg, env).await;
                let mut w = wr.lock().await;
                framed::write_frame(&mut *w, &reply).await?;
            }

            Frame::Request(Request::Subscribe) => {
                // Send the current fleet, then forward live events on a side task.
                let snapshot = daemon.reg.snapshot_event();
                {
                    let mut w = wr.lock().await;
                    framed::write_frame(&mut *w, &snapshot).await?;
                }
                let mut rx = daemon.reg.subscribe();
                let wr2 = wr.clone();
                tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let mut w = wr2.lock().await;
                                if framed::write_frame(&mut *w, &ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(RecvError::Lagged(n)) => {
                                tracing::warn!("UI lagged, dropped {n} events");
                                continue;
                            }
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
            }

            Frame::Request(req) => handle_request(&daemon, req),
        }
    }
    Ok(())
}

fn handle_request(daemon: &Arc<Daemon>, req: Request) {
    match req {
        Request::Spawn(spec) => {
            if let Err(e) = daemon.spawn(spec) {
                daemon.reg.emit_error(format!("spawn failed: {e}"));
            }
        }
        Request::Input { target, data } => daemon.input(&target, &data),
        Request::Decide {
            session,
            approve,
            instruction,
        } => {
            let decision = if approve {
                HookDecision::Allow
            } else {
                HookDecision::Deny {
                    reason: instruction
                        .clone()
                        .unwrap_or_else(|| "denied by user".into()),
                }
            };
            // Resolve a parked PreToolUse hook, if any.
            if let Some(tx) = daemon.reg.take_pending(&session) {
                let _ = tx.send(decision);
            }
            let activity = if approve {
                "approved — continuing"
            } else {
                "denied — redirecting"
            };
            daemon.reg.set_state(&session, State::Working, activity);
        }
        Request::SetAutonomy { session, level } => daemon.reg.set_autonomy(&session, level),
        Request::SetDefaultAutonomy(level) => daemon.reg.set_default_autonomy(level),
        // Pause/Resume land in Phase 3 (SIGSTOP/SIGCONT on the child group).
        Request::Pause(_) | Request::Resume(_) => {
            tracing::debug!("pause/resume not yet implemented");
        }
        Request::Stop(target) => daemon.stop(&target),
        Request::Resize {
            session,
            cols,
            rows,
        } => daemon.resize(&session, cols, rows),
        // Grid snapshot streaming lands in Phase 1 rendering.
        Request::RequestGrid(_) => {}
        Request::Close(id) => daemon.close(&id),
        Request::Subscribe => {}
    }
}
