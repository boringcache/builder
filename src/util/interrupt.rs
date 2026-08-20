use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};

use crate::ui;

#[derive(Clone)]
struct CleanupAction {
    message: String,
    action: Arc<dyn Fn() + Send + Sync + 'static>,
}

#[derive(Default)]
pub(crate) struct InterruptState {
    interrupted: AtomicBool,
    next_id: AtomicUsize,
    cleanup_actions: Mutex<BTreeMap<usize, CleanupAction>>,
}

impl InterruptState {
    fn mark_interrupted(&self) -> bool {
        !self.interrupted.swap(true, Ordering::SeqCst)
    }

    fn reset(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    fn register_cleanup<F>(&'static self, message: impl Into<String>, action: F) -> Registration
    where
        F: Fn() + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let cleanup = CleanupAction {
            message: message.into(),
            action: Arc::new(action),
        };
        self.cleanup_actions
            .lock()
            .expect("interrupt cleanup registry poisoned")
            .insert(id, cleanup);
        Registration { state: self, id }
    }

    fn unregister_cleanup(&self, id: usize) {
        if let Ok(mut cleanup_actions) = self.cleanup_actions.lock() {
            cleanup_actions.remove(&id);
        }
    }

    fn handle_interrupt(&self) {
        if !self.mark_interrupted() {
            eprintln!(
                "{}",
                ui::warning("second interrupt received; exiting immediately")
            );
            std::process::exit(130);
        }

        eprintln!(
            "{} aborting current work and running cleanup",
            ui::warning("interrupt received:")
        );

        let cleanup_actions = self
            .cleanup_actions
            .lock()
            .map(|actions| actions.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        for cleanup in cleanup_actions {
            if !cleanup.message.trim().is_empty() {
                eprintln!("{} {}", ui::warning("cleanup:"), cleanup.message);
            }
            (cleanup.action)();
        }
    }
}

pub(crate) struct Registration {
    state: &'static InterruptState,
    id: usize,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.state.unregister_cleanup(self.id);
    }
}

pub(crate) fn state() -> Result<&'static InterruptState> {
    static STATE: OnceLock<InterruptState> = OnceLock::new();
    static HANDLER_INSTALL: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    let state = STATE.get_or_init(InterruptState::default);
    let install = HANDLER_INSTALL.get_or_init(|| {
        ctrlc::set_handler(|| {
            if let Some(state) = STATE.get() {
                state.handle_interrupt();
            }
        })
        .map_err(|err| err.to_string())
    });

    match install {
        Ok(()) => Ok(state),
        Err(err) => Err(anyhow!("failed to install interrupt handler: {err}")),
    }
}

pub(crate) fn reset() -> Result<()> {
    state()?.reset();
    Ok(())
}

pub(crate) fn check(context: &str) -> Result<()> {
    if state()?.is_interrupted() {
        bail!("interrupted while {context}");
    }
    Ok(())
}

pub(crate) fn register_cleanup<F>(message: impl Into<String>, action: F) -> Result<Registration>
where
    F: Fn() + Send + Sync + 'static,
{
    Ok(state()?.register_cleanup(message, action))
}
