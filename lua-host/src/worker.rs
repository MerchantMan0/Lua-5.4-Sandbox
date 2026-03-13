use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use lua_protocol::{LuaValue, Request, Response};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Semaphore};
use uuid::Uuid;

use lua_protocol::codec;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("worker not found")]
    NotFound,
    #[error("worker is busy")]
    Busy,
    #[error("worker timed out")]
    Timeout,
    #[error("worker process error: {0}")]
    Crashed(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

type Inflight = (Request, oneshot::Sender<Result<Response, WorkerError>>);

#[derive(Clone)]
pub struct WorkerHandle {
    tx: mpsc::Sender<Inflight>,
    // fix this is a hack (design): blocks concurrent requests instead of queuing.
    // Semaphore(1) enforces one in-flight request at a time per worker.
    // planned to be a queue and is partially implemented.
    in_flight: Arc<Semaphore>,
}

impl WorkerHandle {
    pub async fn exec(&self, script: String) -> Result<Response, WorkerError> {
        self.send(Request::Exec { script }).await
    }

    pub async fn call(&self, function: String, args: Vec<LuaValue>) -> Result<Response, WorkerError> {
        self.send(Request::Call { function, args }).await
    }

    pub async fn ping(&self) -> Result<(), WorkerError> {
        self.send(Request::Ping).await.map(|_| ())
    }

    pub async fn shutdown(self) -> Result<(), WorkerError> {
        self.send(Request::Shutdown).await?;
        Ok(())
    }

    async fn send(&self, req: Request) -> Result<Response, WorkerError> {
        let _permit = self.in_flight
            .try_acquire()
            .map_err(|_| WorkerError::Busy)?;

        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send((req, reply_tx))
            .await
            .map_err(|_| WorkerError::Crashed("worker channel closed".into()))?;

        reply_rx
            .await
            .map_err(|_| WorkerError::Crashed("worker task dropped reply channel".into()))?
    }
}

struct WorkerEntry {
    handle: WorkerHandle,
    _sandbox: TempDir,
}

pub struct WorkerRegistry {
    registry: Arc<DashMap<Uuid, WorkerEntry>>,
    worker_bin: std::path::PathBuf,
    sandbox_root: std::path::PathBuf,
}

impl WorkerRegistry {
    pub fn new(worker_bin: impl AsRef<Path>, sandbox_root: impl AsRef<Path>) -> Self {
        Self {
            registry: Arc::new(DashMap::new()),
            worker_bin: worker_bin.as_ref().to_path_buf(),
            sandbox_root: sandbox_root.as_ref().to_path_buf(),
        }
    }

    pub async fn spawn(&self) -> Result<Uuid, WorkerError> {
        let sandbox = tempfile::Builder::new()
            .prefix("worker-")
            .tempdir_in(&self.sandbox_root)
            .context("create sandbox directory")?;

        let sandbox_dir = sandbox.path().to_path_buf();

        let (host_stream, child_stream) =
            UnixStream::pair().context("create socketpair")?;

        let child_fd = child_stream.as_raw_fd();

        let mut cmd = Command::new(&self.worker_bin);
        // fix this is a hack: passing fd as argv string; SCM_RIGHTS or env would be cleaner.
        cmd.arg(child_fd.to_string()).arg(sandbox_dir.as_os_str());

        unsafe {
            // Rust sets O_CLOEXEC on all fds by default
            // clear so child_fd survives exec().
            cmd.pre_exec(move || {
                let flags = libc::fcntl(child_fd, libc::F_GETFD);
                if flags == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let ret = libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                if ret == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().context("spawn lua-worker")?;

        // The child inherited the fd via pre_exec.
        // close the parent's copy so EOF is detectable.
        drop(child_stream);

        // fix this is a hack (design): buffer size 1 combined with Semaphore(1) limits throughput.
        let (tx, rx) = mpsc::channel::<Inflight>(1);
        let id = Uuid::new_v4();
        let handle = WorkerHandle { tx, in_flight: Arc::new(Semaphore::new(1)) };
        self.registry.insert(id, WorkerEntry { handle: handle.clone(), _sandbox: sandbox });
        tokio::spawn(worker_task(host_stream, rx, child, Arc::clone(&self.registry), id));
        Ok(id)
    }

    pub async fn ping(&self, id: Uuid) -> Result<(), WorkerError> {
        let handle = self.registry.get(&id).ok_or(WorkerError::NotFound)?.handle.clone();
        handle.ping().await
    }

    pub async fn exec(&self, id: Uuid, script: String) -> Result<Response, WorkerError> {
        let handle = self.registry.get(&id).ok_or(WorkerError::NotFound)?.handle.clone();
        handle.exec(script).await
    }

    pub async fn call(
        &self,
        id: Uuid,
        function: String,
        args: Vec<LuaValue>,
    ) -> Result<Response, WorkerError> {
        let handle = self.registry.get(&id).ok_or(WorkerError::NotFound)?.handle.clone();
        handle.call(function, args).await
    }

    pub async fn shutdown(&self, id: Uuid) -> Result<(), WorkerError> {
        let (_, entry) = self.registry.remove(&id).ok_or(WorkerError::NotFound)?;
        entry.handle.shutdown().await?;
        Ok(())
    }

    pub fn worker_ids(&self) -> Vec<Uuid> {
        self.registry.iter().map(|r| *r.key()).collect()
    }
}

/// Maximum wall-clock time for worker response
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

async fn worker_task(
    stream: UnixStream,
    mut rx: mpsc::Receiver<Inflight>,
    mut child: Child,
    registry: Arc<DashMap<Uuid, WorkerEntry>>,
    id: Uuid,
) {
    let mut framed = codec::framed(stream);

    loop {
        let (req, reply_tx) = tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => m,
                None => break,
            },
            status = child.wait() => {
                let reason = match status {
                    Ok(s) => format!("worker process exited unexpectedly: {s}"),
                    Err(e) => format!("worker process wait error: {e}"),
                };
                eprintln!("lua-worker[{id}]: {reason}");
                break;
            }
        };

        let is_shutdown = matches!(req, Request::Shutdown);

        let bytes = match rmp_serde::to_vec_named(&req) {
            Ok(b) => b,
            Err(e) => {
                let reason = format!("serialize request: {e}");
                let _ = reply_tx.send(Err(WorkerError::Crashed(reason.clone())));
                break;
            }
        };

        if let Err(e) = framed.send(Bytes::from(bytes)).await {
            let reason = format!("send to worker: {e}");
            let _ = reply_tx.send(Err(WorkerError::Crashed(reason.clone())));
            break;
        }

        if is_shutdown {
            // fix this is a hack: worker exits without response; dummy Ok satisfies API.
            // No response is expected from the worker
            // responds to the server with a dummy Ok to satisfy the API.
            let _ = reply_tx.send(Ok(Response::Ok { values: vec![], console: vec![], gas_remaining: 0, memory_used: 0 }));
            break;
        }

        match tokio::time::timeout(RESPONSE_TIMEOUT, framed.next()).await {
            Err(_elapsed) => {
                eprintln!("lua-worker[{id}]: response timeout after {RESPONSE_TIMEOUT:?}, killing");
                let _ = reply_tx.send(Err(WorkerError::Timeout));
                break;
            }
            Ok(Some(Ok(frame))) => {
                let result = rmp_serde::from_slice::<Response>(&frame)
                    .map_err(|e| WorkerError::Crashed(format!("deserialize response: {e}")));
                let _ = reply_tx.send(result);
            }
            Ok(Some(Err(e))) => {
                let reason = format!("framing error: {e}");
                let _ = reply_tx.send(Err(WorkerError::Crashed(reason.clone())));
                break;
            }
            Ok(None) => {
                let reason = "worker closed connection".to_string();
                let _ = reply_tx.send(Err(WorkerError::Crashed(reason.clone())));
                break;
            }
        }
    }

    registry.remove(&id);

    // Reap the child process to avoid zombies
    let graceful = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    if graceful.is_err() {
        eprintln!("lua-worker[{id}]: did not exit after grace period, sending SIGKILL");
        if let Err(e) = child.kill().await {
            eprintln!("lua-worker[{id}]: kill failed: {e}");
        }
    }
}
