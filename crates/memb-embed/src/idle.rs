//! A lazily loaded value that can be dropped after it has sat unused, so a model that
//! is needed once does not keep its memory for the life of a long-running process.
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct IdleSlot<T> {
    inner: Mutex<(Option<T>, Instant)>,
}

impl<T> Default for IdleSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> IdleSlot<T> {
    pub fn new() -> Self {
        Self { inner: Mutex::new((None, Instant::now())) }
    }

    /// Runs `f` on the value, loading it first if it is not loaded (again, after an
    /// unload). The use time is recorded when `f` returns.
    pub fn with<R>(
        &self,
        load: impl FnOnce() -> Result<T, String>,
        f: impl FnOnce(&mut T) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = self.inner.lock().map_err(|e| e.to_string())?;
        if guard.0.is_none() {
            guard.0 = Some(load()?);
        }
        let result = f(guard.0.as_mut().expect("loaded above"));
        guard.1 = Instant::now();
        result
    }

    /// Drops the value if it was last used at least `idle` before `now`. True if it did.
    pub fn unload_if_idle(&self, idle: Duration, now: Instant) -> bool {
        let Ok(mut guard) = self.inner.lock() else { return false };
        if guard.0.is_some() && now.saturating_duration_since(guard.1) >= idle {
            guard.0 = None;
            return true;
        }
        false
    }

    /// Drops the value now (memory pressure). True if there was one.
    pub fn unload(&self) -> bool {
        self.inner.lock().map(|mut g| g.0.take().is_some()).unwrap_or(false)
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.lock().map(|g| g.0.is_some()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_value_unloads_after_the_idle_time_and_loads_again_on_next_use() {
        let slot: IdleSlot<u32> = IdleSlot::new();
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(7u32)
        };
        assert!(!slot.is_loaded());
        assert_eq!(slot.with(load, |v| Ok(*v)).unwrap(), 7);
        assert_eq!(slot.with(load, |v| Ok(*v + 1)).unwrap(), 8);
        assert_eq!(loads.get(), 1, "loaded once while in use");
        let now = Instant::now();
        assert!(!slot.unload_if_idle(Duration::from_secs(300), now), "just used: kept");
        assert!(slot.is_loaded());
        assert!(!slot.unload_if_idle(Duration::from_secs(300), now + Duration::from_secs(299)));
        assert!(slot.unload_if_idle(Duration::from_secs(300), now + Duration::from_secs(300)), "idle: dropped");
        assert!(!slot.is_loaded());
        assert!(!slot.unload_if_idle(Duration::from_secs(300), now + Duration::from_secs(900)), "nothing to drop");
        assert_eq!(slot.with(load, |v| Ok(*v)).unwrap(), 7);
        assert_eq!(loads.get(), 2, "reloaded after the unload");
        assert!(slot.unload());
        assert!(!slot.unload());
    }

    #[test]
    fn a_failed_load_leaves_the_slot_empty_and_a_failed_use_still_counts_as_use() {
        let slot: IdleSlot<u32> = IdleSlot::new();
        assert!(slot.with(|| Err("no model".into()), |v| Ok(*v)).is_err());
        assert!(!slot.is_loaded());
        assert!(slot.with(|| Ok(1u32), |_| Err::<(), _>("bad input".into())).is_err());
        assert!(slot.is_loaded(), "the loaded model is kept after one bad request");
    }
}
