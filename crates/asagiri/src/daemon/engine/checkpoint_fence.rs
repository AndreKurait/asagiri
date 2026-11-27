use super::{Container, Engine, Error, SHUTTING_DOWN, Status};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

impl Container {
    pub(super) fn ensure_unfenced(&self) -> Result<(), Error> {
        if let Some(name) = &self.checkpoint_fence {
            return Err(Error::Conflict(format!("container {} is fenced for checkpoint {name}", self.name)));
        }
        Ok(())
    }
}

impl Engine {
    /// Fence only after all named captures exist and all members are still paused.
    /// Releasing a fence never starts or thaws a workload.
    pub fn checkpoint_fence(&self, name: &str, keys: &[String], release: bool) -> Result<(), Error> {
        if keys.is_empty() { return Err(Error::BadRequest("supply at least one source container".into())); }
        // Power transitions can thaw ordinary paused containers during shutdown.
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        if SHUTTING_DOWN.load(Ordering::SeqCst) { return Err(Error::Conflict("daemon is shutting down".into())); }
        let mut members = BTreeMap::new();
        {
            let state = self.lock();
            for key in keys {
                let id = super::resolve(&state, key)?;
                let lifecycle = state.containers[&id].lifecycle.clone();
                if members.insert(id, lifecycle).is_some() { return Err(Error::BadRequest("duplicate source container".into())); }
            }
        }
        // Consistent ID order excludes concurrent pause, start, stop and removal.
        let _guards: Vec<_> = members.values().map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner())).collect();
        // The same writer lock as every save keeps a failed transition from leaking into another snapshot.
        let _writing = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.lock();
        for id in members.keys() {
            let c = state.containers.get(id).ok_or_else(|| Error::NotFound(id.clone()))?;
            if c.checkpoint_fence.as_deref().is_some_and(|saved| saved != name) { c.ensure_unfenced()?; }
            if c.checkpoint_fence.as_deref() == Some(name) { continue; }
            if !release && (c.spec.auto_remove || c.status != Status::Running || !c.paused) {
                return Err(Error::Conflict(format!("container {} must be paused and retained to fence its checkpoint", c.name)));
            }
            if !release && c.paused_checkpoint.as_deref() != Some(name) {
                return Err(Error::Conflict(format!("capture checkpoint {name} on {} during its current pause", c.name)));
            }
        }
        let mut previously_fenced = Vec::new();
        for id in members.keys() {
            let c = state.containers.get_mut(id).expect("members validated under the state lock");
            if release && c.checkpoint_fence.is_some() { previously_fenced.push(id.clone()); }
            c.checkpoint_fence = (!release).then(|| name.to_owned());
            if !release { c.restart_pending = false; }
        }
        let snapshot = Self::encode_records(&state);
        drop(state);
        if let Err(e) = snapshot.and_then(|(json, _)| self.write_records(&json, true)) {
            // A failed release must keep every affected source fenced in this daemon.
            if release {
                let mut state = self.lock();
                for id in &previously_fenced {
                    state.containers.get_mut(id).expect("member still locked").checkpoint_fence = Some(name.to_owned());
                }
            }
            return Err(Error::Io(std::io::Error::other(format!("could not persist checkpoint fence; retry before handoff: {e}"))));
        }
        Ok(())
    }
}
