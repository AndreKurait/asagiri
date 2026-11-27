use super::{Container, Engine, Error, SHUTTING_DOWN, Status};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;

impl Container {
    pub(super) fn ensure_unfenced(&self) -> Result<(), Error> {
        if let Some(name) = &self.checkpoint_fence {
            return Err(Error::Conflict(format!("container {} is fenced for checkpoint {name}", self.name)));
        }
        Ok(())
    }
}

pub enum SourceAction {
    Capture { external_network: bool },
    Fence,
    Release,
    Retire,
}

struct CaptureMember<'a> {
    id: &'a str,
    paused: bool,
    previous_capture: Option<String>,
    volumes: String,
    network: String,
}

impl Engine {
    fn checkpoint_members(&self, keys: &[String], role: &str) -> Result<BTreeMap<String, Arc<Mutex<u64>>>, Error> {
        let mut members = BTreeMap::new();
        let state = self.lock();
        for key in keys {
            let id = super::resolve(&state, key)?;
            let lifecycle = state.containers[&id].lifecycle.clone();
            if members.insert(id, lifecycle).is_some() {
                return Err(Error::BadRequest(format!("duplicate {role}")));
            }
        }
        Ok(members)
    }

    pub fn activate_checkpoint_receivers(&self, keys: &[String]) -> Result<(), Error> {
        if keys.is_empty() || keys.len() > asagiri_proto::MAX_FRAME.saturating_sub(32) / 68 {
            return Err(Error::BadRequest("invalid checkpoint receiver group size".into()));
        }
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        if SHUTTING_DOWN.load(Ordering::SeqCst) { return Err(Error::Conflict("daemon is shutting down".into())); }
        let members = self.checkpoint_members(keys, "checkpoint receiver")?;
        let _guards: Vec<_> = members.values().map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner())).collect();
        for id in members.keys() {
            self.with(id, |c| {
                c.ensure_unfenced()?;
                if c.status != Status::Running || !c.in_guest || !c.spec.checkpoint_wait || c.paused {
                    return Err(Error::Conflict(format!("container {id} is not a running checkpoint receiver")));
                }
                Ok(())
            })??;
        }
        let ids: Vec<_> = members.keys().cloned().collect();
        let vm = self.awake().ok_or_else(|| Error::Conflict("receiver guest stopped".into()))?;
        vm.activate_checkpoint_receivers(&ids).map_err(|error| Error::Conflict(error.to_string()))
    }

    pub fn checkpoint_group(&self, name: &str, keys: &[String], action: SourceAction) -> Result<(), Error> {
        if keys.is_empty() { return Err(Error::BadRequest("supply at least one source container".into())); }
        // Power transitions can thaw ordinary paused containers during shutdown.
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        if SHUTTING_DOWN.load(Ordering::SeqCst) { return Err(Error::Conflict("daemon is shutting down".into())); }
        let members = self.checkpoint_members(keys, "source container")?;
        // Consistent ID order excludes concurrent pause, start, stop and removal.
        let _guards: Vec<_> = members.values().map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner())).collect();
        let ids: Vec<_> = members.keys().cloned().collect();
        match action {
            SourceAction::Capture { external_network } => self.capture_group_locked(name, &ids, external_network),
            SourceAction::Fence => self.persist_fence(name, &ids, false),
            SourceAction::Release => self.persist_fence(name, &ids, true),
            SourceAction::Retire => self.retire_checkpoint_sources(name, &ids),
        }
    }

    fn retire_checkpoint_sources(&self, name: &str, ids: &[String]) -> Result<(), Error> {
        let running = {
            let _writing = self.saving.lock().unwrap_or_else(|e| e.into_inner());
            let mut state = self.lock();
            let mut running = Vec::new();
            for id in ids {
                let c = state.containers.get(id).ok_or_else(|| Error::NotFound(id.clone()))?;
                if c.checkpoint_fence.as_deref() != Some(name) {
                    return Err(Error::Conflict(format!("source {id} is not fenced for checkpoint {name}")));
                }
                match c.status {
                    Status::Running if c.paused && c.in_guest => running.push(id),
                    Status::Exited => {}
                    _ => return Err(Error::Conflict(format!("source {id} is neither paused nor stopped"))),
                }
            }
            for id in ids {
                state.containers.get_mut(id).expect("source still locked").checkpoint_retired = true;
            }
            let snapshot = Self::encode_records(&state)?;
            drop(state);
            // Persist the irreversible transition before terminating any processes.
            self.write_records(&snapshot.0, true)?;
            running
        };
        if !running.is_empty() {
            let vm = self.awake().ok_or_else(|| Error::Conflict("source guest stopped during retirement".into()))?;
            for id in running { vm.retire_checkpoint_source(id)?; }
        }
        let timeout = std::time::Duration::from_secs(10);
        let state = self.lock();
        let (_state, wait) = self.changed.wait_timeout_while(state, timeout, |state| {
            ids.iter().any(|id| state.containers.get(id).is_some_and(|c| c.status != Status::Exited))
        }).unwrap_or_else(|error| error.into_inner());
        if wait.timed_out() {
            return Err(Error::Conflict("source exit was not confirmed after retirement".into()));
        }
        Ok(())
    }

    fn capture_group_locked(&self, name: &str, ids: &[String], external_network: bool) -> Result<(), Error> {
        let vm = self.vm()?;
        if !vm.live_checkpoint {
            return Err(Error::Conflict("source checkpoint runtime is unavailable".into()));
        }
        let mut configs = Vec::with_capacity(ids.len());
        for id in ids {
            let (paused, previous_capture, auto_remove) = self.with(id, |c| (c.paused, c.paused_checkpoint.clone(), c.spec.auto_remove))?;
            if auto_remove { return Err(Error::Conflict(format!("source container {id} must be retained"))); }
            let (volumes, network) = self.checkpoint_config(id, name, external_network)?;
            if vm.checkpoints(id)?.iter().any(|saved| saved == name) {
                return Err(Error::Conflict(format!("checkpoint {name} already exists on {id}")));
            }
            configs.push(CaptureMember { id, paused, previous_capture, volumes, network });
        }

        let mut frozen = Vec::new();
        let mut captured = Vec::new();
        let capture = (|| {
            for member in &configs {
                if !member.paused {
                    self.pause_locked(member.id, true)?;
                    frozen.push(member.id);
                }
            }
            for member in &configs {
                self.capture_prepared(member.id, name, external_network, &member.volumes, &member.network)?;
                captured.push(member);
            }
            self.persist_fence(name, ids, false)
        })();
        if let Err(error) = capture {
            // A durability error leaves the in-memory fence installed. The source must stay frozen
            // until a fence retry succeeds, even when the final filesystem sync was uncertain.
            {
                let state = self.lock();
                if ids.iter().any(|id| state.containers.get(id).is_some_and(|c| c.checkpoint_fence.as_deref() == Some(name))) {
                    return Err(error);
                }
            }
            let mut failures = Vec::new();
            for member in captured.into_iter().rev() {
                let id = member.id;
                // Discard only captures acknowledged during this request. All lifecycle locks are still held.
                let discarded = self.open_copy(id, asagiri_proto::copy_op::CHECKPOINT_DISCARD, name, 0)
                    .and_then(|stream| Self::restore_reply(&stream));
                if let Err(e) = discarded { failures.push(format!("discard {id}/{name}: {e}")); }
                if let Some(c) = self.lock().containers.get_mut(id) { c.paused_checkpoint.clone_from(&member.previous_capture); }
            }
            for id in frozen.into_iter().rev() {
                if let Err(e) = self.pause_locked(id, false) { failures.push(format!("unpause {id}: {e}")); }
            }
            if failures.is_empty() { return Err(error); }
            return Err(Error::Io(std::io::Error::other(format!("{error}; incomplete source rollback: {}", failures.join(", ")))));
        }
        Ok(())
    }

    fn persist_fence(&self, name: &str, ids: &[String], release: bool) -> Result<(), Error> {
        // The same writer lock as every save keeps a failed transition from leaking into another snapshot.
        let _writing = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.lock();
        for id in ids {
            let c = state.containers.get(id).ok_or_else(|| Error::NotFound(id.clone()))?;
            if release && c.checkpoint_retired {
                return Err(Error::Conflict(format!("checkpoint source {} has been retired", c.name)));
            }
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
        for id in ids {
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
