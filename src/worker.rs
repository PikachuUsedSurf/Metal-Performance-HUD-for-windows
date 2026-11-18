//! Runs slow jobs (talking to the device, downloading) in the background.
//!
//! The window must redraw about 60 times a second, so it can't wait for a device to
//! answer. Instead each button press starts a background "task" that sends messages
//! back to the window through a channel (a thread-safe queue).

use std::future::Future;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::device::{AppInfo, DeviceInfo, DeviceStatus};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Info,
    Success,
    Error,
}

/// A message from a background task to the window.
pub enum Msg {
    Log(LogKind, String),
    Devices(Vec<DeviceInfo>),
    Status(String, DeviceStatus),
    Apps(Vec<AppInfo>),
    Progress(Option<(String, f32)>),
    /// The background task finished (successfully or not).
    Finished,
}

/// Handed to background tasks so they can report back to the window.
#[derive(Clone)]
pub struct Reporter {
    tx: Sender<Msg>,
    ctx: eframe::egui::Context,
}

impl Reporter {
    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint(); // wake the window up so it shows the message
    }
    pub fn log(&self, text: impl Into<String>) {
        self.send(Msg::Log(LogKind::Info, text.into()));
    }
    pub fn success(&self, text: impl Into<String>) {
        self.send(Msg::Log(LogKind::Success, text.into()));
    }
    pub fn error(&self, text: impl Into<String>) {
        self.send(Msg::Log(LogKind::Error, text.into()));
    }
    pub fn progress(&self, label: &str, fraction: f32) {
        self.send(Msg::Progress(Some((label.to_owned(), fraction.clamp(0.0, 1.0)))));
    }
    pub fn clear_progress(&self) {
        self.send(Msg::Progress(None));
    }
}

pub struct Worker {
    reporter: Reporter,
    pub inbox: Receiver<Msg>,
}

impl Worker {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (tx, inbox) = channel();
        Self { reporter: Reporter { tx, ctx }, inbox }
    }

    /// Starts a background job on its own thread. Any error it returns is shown in the log.
    ///
    /// Each job gets a small single-threaded async runtime. Jobs run one at a time
    /// (the buttons are disabled while one is busy), so this is simple and plenty fast.
    pub fn spawn<F, Fut>(&self, job: F)
    where
        F: FnOnce(Reporter) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + 'static,
    {
        let rep = self.reporter.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            match runtime {
                Ok(runtime) => {
                    if let Err(e) = runtime.block_on(job(rep.clone())) {
                        rep.error(format!("{e:#}"));
                    }
                }
                Err(e) => rep.error(format!("Couldn't start a background job: {e}")),
            }
            rep.clear_progress();
            rep.send(Msg::Finished);
        });
    }
}

#[cfg(test)]
impl Reporter {
    /// A reporter for tests that just collects messages.
    pub fn for_tests() -> (Self, Receiver<Msg>) {
        let (tx, rx) = channel();
        (Self { tx, ctx: eframe::egui::Context::default() }, rx)
    }
}
