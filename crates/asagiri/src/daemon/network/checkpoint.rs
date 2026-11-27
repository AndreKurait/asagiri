use super::Networks;
use crate::daemon::api::{network_create_settings, network_definition};
use crate::daemon::engine::Error;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::{fs::File, io};

fn configuration(value: Value) -> Result<serde_json::Map<String, Value>, Error> {
    let Value::Object(mut obj) = value else { return Err(Error::BadRequest("invalid checkpoint network definition".into())); };
    obj.remove("Id");
    obj.remove("Created");
    Ok(obj)
}

impl Networks {
    /// The network registry is locked for the entire group. Until the rename,
    /// every change is an append that can be discarded without touching existing endpoints.
    pub(in crate::daemon) fn prepare_checkpoint_networks(&mut self, networks: Vec<Value>) -> Result<Value, Error> {
        let initial = self.list.len();
        let mut prepared = BTreeMap::new();
        let result = (|| {
            for source in networks {
                let source = configuration(source)?;
                let name = source.get("Name").and_then(Value::as_str).filter(|n| !n.is_empty()).ok_or_else(|| Error::BadRequest("checkpoint network name is missing".into()))?;
                let existing = self.list.iter().position(|n| n.name == name);
                let index = match existing {
                    Some(index) => index,
                    None if source.get("Driver").and_then(Value::as_str) == Some("bridge") && !matches!(name, "bridge" | "default" | "host" | "none") => {
                        let n = self.new_network(name, network_create_settings(&Value::Object(source.clone()))?).map_err(Error::Conflict)?;
                        self.list.push(n);
                        self.list.len() - 1
                    }
                    None => return Err(Error::Conflict(format!("network {name} requires a matching destination network"))),
                };
                let dest = &self.list[index];
                let actual = configuration(network_definition(dest))?;
                if actual != source {
                    let keys: BTreeSet<_> = source.keys().chain(actual.keys()).filter(|key| source.get(*key) != actual.get(*key)).map(String::as_str).collect();
                    return Err(Error::Conflict(format!("destination network {name} differs in {}", keys.into_iter().collect::<Vec<_>>().join(", "))));
                }
                prepared.insert(name.to_owned(), json!({"id": dest.id, "created": index >= initial}));
            }
            if self.list.len() != initial { self.write_networks(true)?; }
            Ok(())
        })();
        if let Err(error) = result { self.list.truncate(initial); return Err(error); }
        // A retry also completes the directory sync if a previous sync failed after publication.
        let parent = self.path.parent().ok_or_else(|| io::Error::other("missing network store directory"))?;
        File::open(parent)?.sync_all()?;
        Ok(json!(prepared))
    }
}
