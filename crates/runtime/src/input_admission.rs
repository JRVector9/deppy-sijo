//! Local permission held through the actual PTY queue write, never across event wakes.
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct InputPermit(Arc<Mutex<bool>>);

impl InputPermit {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(true)))
    }

    /// After this returns, this permit can admit no further input.
    pub fn revoke(&self) {
        if let Ok(mut allowed) = self.0.lock() {
            *allowed = false;
        }
    }
}

impl Default for InputPermit {
    fn default() -> Self {
        Self::new()
    }
}

pub struct InputAdmission {
    permit: InputPermit,
    deadline: Instant,
    authorize: Box<dyn Fn(&mut dyn FnMut()) + Send + Sync>,
}

impl InputAdmission {
    pub fn new(
        permit: InputPermit,
        deadline: Instant,
        authorize: impl Fn(&mut dyn FnMut()) + Send + Sync + 'static,
    ) -> Self {
        Self {
            permit,
            deadline,
            authorize: Box::new(authorize),
        }
    }

    pub(crate) fn admit<T>(&self, write: impl FnOnce() -> T) -> Option<T> {
        let allowed = self.permit.0.lock().ok()?;
        if !*allowed || Instant::now() >= self.deadline {
            return None;
        }
        let mut write = Some(write);
        let mut result = None;
        (self.authorize)(&mut || {
            // The authorization callback may have waited for its own lock.
            if Instant::now() < self.deadline
                && let Some(write) = write.take()
            {
                result = Some(write());
            }
        });
        drop(allowed);
        result
    }
}
