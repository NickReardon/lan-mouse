//! One OS clipboard worker; native calls and retries never block the input loop.
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
use tokio::sync::oneshot;

#[derive(Debug)]
pub(super) struct Snapshot {
    pub text: Option<String>,
    pub revision: u64,
}

enum Request {
    Revision(oneshot::Sender<u64>),
    Read(oneshot::Sender<io::Result<Snapshot>>),
    Write {
        text: String,
        revision: u64,
        epoch: u64,
        reply: oneshot::Sender<io::Result<bool>>,
    },
}

#[derive(Clone)]
pub(super) struct ClipboardWorker(mpsc::SyncSender<Request>);

trait Backend: Send + 'static {
    fn revision(&mut self) -> u64;
    fn read(&mut self) -> io::Result<Snapshot>;
    fn write(&mut self, text: String) -> io::Result<()>;
}

struct Native;
impl Backend for Native {
    fn revision(&mut self) -> u64 {
        native_revision()
    }
    fn read(&mut self) -> io::Result<Snapshot> {
        read()
    }
    fn write(&mut self, text: String) -> io::Result<()> {
        write(text)
    }
}

impl ClipboardWorker {
    #[cfg(test)]
    pub fn memory(text: Option<&str>, epoch: Arc<AtomicU64>) -> Self {
        struct Memory(Snapshot);
        impl Backend for Memory {
            fn revision(&mut self) -> u64 {
                self.0.revision
            }
            fn read(&mut self) -> io::Result<Snapshot> {
                Ok(Snapshot {
                    text: self.0.text.clone(),
                    revision: self.0.revision,
                })
            }
            fn write(&mut self, text: String) -> io::Result<()> {
                self.0.text = Some(text);
                self.0.revision += 1;
                Ok(())
            }
        }
        Self::spawn(
            epoch,
            Memory(Snapshot {
                text: text.map(str::to_owned),
                revision: 0,
            }),
        )
        .unwrap()
    }

    pub fn new(epoch: Arc<AtomicU64>) -> io::Result<Self> {
        if !cfg!(any(windows, target_os = "macos")) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "clipboard sharing supports Windows and macOS",
            ));
        }
        Self::spawn(epoch, Native)
    }

    fn spawn(epoch: Arc<AtomicU64>, mut backend: impl Backend) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(8);
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Revision(reply) => {
                            let _ = reply.send(backend.revision());
                        }
                        Request::Read(reply) => {
                            let _ = reply.send(backend.read());
                        }
                        Request::Write {
                            text,
                            revision,
                            epoch: expected,
                            reply,
                        } => {
                            let result = if reply.is_closed()
                                || epoch.load(Ordering::SeqCst) != expected
                                || backend.revision() != revision
                            {
                                Ok(false)
                            } else {
                                backend.write(text).map(|()| true)
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
            })?;
        Ok(Self(tx))
    }

    pub async fn read(&self) -> io::Result<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.0
            .try_send(Request::Read(tx))
            .map_err(io::Error::other)?;
        rx.await.map_err(io::Error::other)?
    }

    pub async fn revision(&self) -> io::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.0
            .try_send(Request::Revision(tx))
            .map_err(io::Error::other)?;
        rx.await.map_err(io::Error::other)
    }

    pub async fn write(&self, text: String, revision: u64, epoch: u64) -> io::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.0
            .try_send(Request::Write {
                text,
                revision,
                epoch,
                reply: tx,
            })
            .map_err(io::Error::other)?;
        rx.await.map_err(io::Error::other)?
    }
}

#[cfg(windows)]
fn native_revision() -> u64 {
    // A process-wide clipboard generation; no clipboard lock is required.
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64 }
}

#[cfg(target_os = "macos")]
fn native_revision() -> u64 {
    objc2::rc::autoreleasepool(|_| {
        objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn native_revision() -> u64 {
    0
}

#[cfg(any(windows, target_os = "macos"))]
fn read() -> io::Result<Snapshot> {
    for _ in 0..3 {
        let revision = native_revision();
        let result = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text());
        if native_revision() != revision {
            continue;
        }
        match result {
            // the configured limit is applied when sending; this only
            // bounds what is read at all
            Ok(text) if text.len() <= super::MAX_SIZE_MIB as usize * super::protocol::MIB => {
                return Ok(Snapshot {
                    text: Some(text),
                    revision,
                });
            }
            Ok(_) => return Err(io::Error::other("clipboard text exceeds 16 MiB")),
            Err(arboard::Error::ContentNotAvailable) => {
                return Ok(Snapshot {
                    text: None,
                    revision,
                });
            }
            Err(arboard::Error::ClipboardOccupied) => {
                std::thread::sleep(std::time::Duration::from_millis(30))
            }
            Err(error) => return Err(io::Error::other(error)),
        }
    }
    Err(io::Error::other("clipboard is busy or changed during read"))
}

#[cfg(any(windows, target_os = "macos"))]
fn write(text: String) -> io::Result<()> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(io::Error::other)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn read() -> io::Result<Snapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard unavailable",
    ))
}
#[cfg(not(any(windows, target_os = "macos")))]
fn write(_: String) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_copy_and_cancelled_switch_preserve_clipboard() {
        let epoch = Arc::new(AtomicU64::new(0));
        let worker = ClipboardWorker::memory(Some("original"), epoch.clone());
        let revision = worker.revision().await.unwrap();
        assert!(
            worker
                .write("new local copy".into(), revision, 0)
                .await
                .unwrap()
        );
        assert!(
            !worker
                .write("stale remote".into(), revision, 0)
                .await
                .unwrap()
        );
        let revision = worker.revision().await.unwrap();
        epoch.fetch_add(1, Ordering::SeqCst);
        assert!(
            !worker
                .write("cancelled transfer".into(), revision, 0)
                .await
                .unwrap()
        );
        assert_eq!(
            worker.read().await.unwrap().text.as_deref(),
            Some("new local copy")
        );
    }
}
