use codex_protocol::protocol::ToolManifestDeltaEntry;
use codex_protocol::protocol::ToolManifestDeltaRemoval;
use codex_protocol::protocol::ToolManifestItem;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(serde::Serialize)]
struct FullManifestRef<'a> {
    hash: &'a str,
    manifest: &'a Value,
}

fn serialized_bytes(value: &impl serde::Serialize) -> Option<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("manifest byte count overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolManifestDictionary {
    manifests: HashMap<String, Value>,
    current_hash: Option<String>,
}

impl ToolManifestDictionary {
    pub fn manifests(&self) -> &HashMap<String, Value> {
        &self.manifests
    }

    pub fn current_hash(&self) -> Option<&str> {
        self.current_hash.as_deref()
    }

    pub fn manifest(&self, hash: &str) -> Option<&Value> {
        self.manifests.get(hash)
    }

    /// Applies a persisted definition, delta, or reference and retains the full
    /// reconstructed manifest under its hash.
    pub fn apply(&mut self, item: &ToolManifestItem) -> Result<(), String> {
        let manifest = if let Some(manifest) = &item.manifest {
            manifest.clone()
        } else if let Some(base_hash) = &item.base_hash {
            let base = self
                .manifests
                .get(base_hash)
                .ok_or_else(|| format!("tool manifest base hash {base_hash} is unavailable"))?;
            apply_delta(base, &item.added, &item.removed)?
        } else {
            if !self.manifests.contains_key(&item.hash) {
                return Err(format!(
                    "tool manifest reference hash {} is unavailable",
                    item.hash
                ));
            }
            self.current_hash = Some(item.hash.clone());
            return Ok(());
        };

        if let Some(existing) = self.manifests.get(&item.hash)
            && existing != &manifest
        {
            return Err(format!(
                "tool manifest hash {} resolves to conflicting definitions",
                item.hash
            ));
        }
        self.manifests.insert(item.hash.clone(), manifest);
        self.current_hash = Some(item.hash.clone());
        Ok(())
    }

    /// Encodes a full runtime snapshot as one definition per hash and compact
    /// references thereafter. New definitions use a delta when the current
    /// manifest has the supported collection shape.
    pub fn encode(&mut self, hash: String, manifest: Value) -> Result<ToolManifestItem, String> {
        if self
            .manifests
            .get(&hash)
            .is_some_and(|existing| existing != &manifest)
        {
            return Err(format!(
                "tool manifest hash {hash} resolves to conflicting definitions"
            ));
        }
        Ok(self.encode_checked(hash, manifest))
    }

    fn encode_checked(&mut self, hash: String, manifest: Value) -> ToolManifestItem {
        if self.manifests.contains_key(&hash) {
            self.current_hash = Some(hash.clone());
            return ToolManifestItem::reference(hash);
        }

        let delta = self
            .current_hash
            .as_ref()
            .and_then(|base_hash| {
                let base = self.manifests.get(base_hash)?;
                let (added, removed) = compute_delta(base, &manifest)?;
                Some(ToolManifestItem::delta(
                    hash.clone(),
                    base_hash.clone(),
                    added,
                    removed,
                ))
            });
        let encoded = if let Some(delta) = delta
            && serialized_bytes(&delta).zip(serialized_bytes(&FullManifestRef {
                hash: &hash, manifest: &manifest,
            })).is_some_and(|(delta, full)| delta < full)
        {
            delta
        } else {
            ToolManifestItem::full(hash.clone(), manifest.clone())
        };
        self.manifests.insert(hash.clone(), manifest);
        self.current_hash = Some(hash);
        encoded
    }

    pub fn encode_item(&mut self, item: &ToolManifestItem) -> Result<ToolManifestItem, String> {
        if item.is_reference() {
            if !self.manifests.contains_key(&item.hash) {
                return Err(format!(
                    "tool manifest reference hash {} is unavailable",
                    item.hash
                ));
            }
            self.current_hash = Some(item.hash.clone());
            return Ok(item.clone());
        }

        if let Some(manifest) = item.manifest.as_ref() {
            if let Some(existing) = self.manifests.get(&item.hash) {
                if existing != manifest {
                    return Err(format!(
                        "tool manifest hash {} resolves to conflicting definitions",
                        item.hash
                    ));
                }
                self.current_hash = Some(item.hash.clone());
                return Ok(ToolManifestItem::reference(item.hash.clone()));
            }

            return self.encode(item.hash.clone(), manifest.clone());
        }

        let base_hash = item
            .base_hash
            .as_ref()
            .ok_or_else(|| "tool manifest delta has no base hash".to_string())?;
        let base = self
            .manifests
            .get(base_hash)
            .ok_or_else(|| format!("tool manifest base hash {base_hash} is unavailable"))?;
        let manifest = apply_delta(base, &item.added, &item.removed)?;
        self.encode(item.hash.clone(), manifest)
    }
}

#[derive(Clone)]
struct NamedEntry<'a> {
    name: String,
    value: &'a Value,
}

fn named_entries(values: &[Value]) -> Vec<NamedEntry<'_>> {
    let mut occurrences = HashMap::<String, usize>::new();
    values
        .iter()
        .map(|value| {
            let base_name = value
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    value
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|kind| format!("@type:{kind}"))
                })
                .unwrap_or_else(|| "@anonymous".to_string());
            let occurrence = occurrences.entry(base_name.clone()).or_default();
            let name = if *occurrence == 0 {
                base_name
            } else {
                format!("{base_name}#{occurrence}")
            };
            *occurrence += 1;
            NamedEntry {
                name,
                value,
            }
        })
        .collect()
}

fn compute_delta(
    base: &Value,
    target: &Value,
) -> Option<(Vec<ToolManifestDeltaEntry>, Vec<ToolManifestDeltaRemoval>)> {
    let base = base.as_object()?;
    let target = target.as_object()?;
    let keys = base
        .keys()
        .chain(target.keys())
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    let mut remaining_cells = 1_000_000usize;
    for key in keys {
        let base_value = base.get(key)?;
        let target_value = target.get(key)?;
        if base_value == target_value {
            continue;
        }
        match (base_value.as_array(), target_value.as_array()) {
            (Some(base_values), Some(target_values)) => {
                let cells = base_values
                    .len()
                    .checked_add(1)?
                    .checked_mul(target_values.len().checked_add(1)?)?;
                remaining_cells = remaining_cells.checked_sub(cells)?;
                let base_entries = named_entries(base_values);
                let target_entries = named_entries(target_values);
                for entries in [&base_entries, &target_entries] {
                    let mut names = HashSet::new();
                    if entries.iter().any(|entry| !names.insert(&entry.name)) {
                        return None;
                    }
                }
                let matches = longest_common_subsequence(&base_entries, &target_entries);
                let matched_base = matches
                    .iter()
                    .map(|(base_index, _)| *base_index)
                    .collect::<HashSet<_>>();
                let matched_target = matches
                    .iter()
                    .map(|(_, target_index)| *target_index)
                    .collect::<HashSet<_>>();

                removed.extend(
                    base_entries
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| !matched_base.contains(index))
                        .map(|(_, entry)| ToolManifestDeltaRemoval {
                            collection: key.to_string(),
                            name: entry.name.clone(),
                        }),
                );
                added.extend(
                    target_entries
                        .into_iter()
                        .enumerate()
                        .filter(|(index, _)| !matched_target.contains(index))
                        .map(|(index, entry)| ToolManifestDeltaEntry {
                            collection: key.to_string(),
                            name: entry.name,
                            index,
                            value: entry.value.clone(),
                        }),
                );
            }
            (None, None) if base_value == target_value => {}
            _ => return None,
        }
    }

    added.sort_by(|left, right| {
        left.collection
            .cmp(&right.collection)
            .then(left.index.cmp(&right.index))
    });
    removed.sort_by(|left, right| {
        left.collection
            .cmp(&right.collection)
            .then(left.name.cmp(&right.name))
    });
    Some((added, removed))
}

fn longest_common_subsequence(base: &[NamedEntry<'_>], target: &[NamedEntry<'_>]) -> Vec<(usize, usize)> {
    let mut lengths = vec![vec![0usize; target.len() + 1]; base.len() + 1];
    for base_index in (0..base.len()).rev() {
        for target_index in (0..target.len()).rev() {
            lengths[base_index][target_index] = if base[base_index].name
                == target[target_index].name
                && base[base_index].value == target[target_index].value
            {
                lengths[base_index + 1][target_index + 1] + 1
            } else {
                lengths[base_index + 1][target_index].max(lengths[base_index][target_index + 1])
            };
        }
    }

    let mut matches = Vec::new();
    let (mut base_index, mut target_index) = (0, 0);
    while base_index < base.len() && target_index < target.len() {
        if base[base_index].name == target[target_index].name
            && base[base_index].value == target[target_index].value
        {
            matches.push((base_index, target_index));
            base_index += 1;
            target_index += 1;
        } else if lengths[base_index + 1][target_index] >= lengths[base_index][target_index + 1] {
            base_index += 1;
        } else {
            target_index += 1;
        }
    }
    matches
}

fn apply_delta(
    base: &Value,
    added: &[ToolManifestDeltaEntry],
    removed: &[ToolManifestDeltaRemoval],
) -> Result<Value, String> {
    let mut manifest = base
        .as_object()
        .cloned()
        .ok_or_else(|| "tool manifest delta base must be an object".to_string())?;
    let collections = added
        .iter()
        .map(|entry| entry.collection.as_str())
        .chain(removed.iter().map(|entry| entry.collection.as_str()))
        .collect::<std::collections::BTreeSet<_>>();

    for collection in collections {
        let values = manifest
            .get(collection)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("tool manifest collection {collection} is unavailable"))?;
        let names = named_entries(values)
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        let unique_names = names.iter().collect::<HashSet<_>>();
        if unique_names.len() != names.len() {
            return Err(format!(
                "ambiguous tool manifest identities in {collection}"
            ));
        }
        let mut removal_ids = HashSet::new();
        for entry in removed
            .iter()
            .filter(|entry| entry.collection == collection)
        {
            if !removal_ids.insert(&entry.name) || !unique_names.contains(&entry.name) {
                return Err(format!(
                    "invalid tool manifest removal {collection}/{}",
                    entry.name
                ));
            }
        }
        let removed_names = removed
            .iter()
            .filter(|entry| entry.collection == collection)
            .map(|entry| entry.name.as_str())
            .collect::<HashSet<_>>();
        let mut values = values
            .iter()
            .zip(names)
            .filter(|(_, name)| !removed_names.contains(name.as_str()))
            .map(|(value, _)| value.clone())
            .collect::<Vec<_>>();
        let mut additions = added
            .iter()
            .filter(|entry| entry.collection == collection)
            .collect::<Vec<_>>();
        additions.sort_by_key(|entry| entry.index);
        let mut indices = HashSet::new();
        let mut identities = HashSet::new();
        for entry in &additions {
            if !indices.insert(entry.index) || !identities.insert(&entry.name) {
                return Err(format!("duplicate tool manifest addition in {collection}"));
            }
            if entry.index > values.len() {
                return Err(format!(
                    "tool manifest delta index {} exceeds collection {collection} length {}",
                    entry.index,
                    values.len()
                ));
            }
            values.insert(entry.index, entry.value.clone());
        }
        let reconstructed = named_entries(&values);
        let mut final_names = HashSet::new();
        if reconstructed
            .iter()
            .any(|entry| !final_names.insert(&entry.name))
            || additions
                .iter()
                .any(|entry| reconstructed[entry.index].name != entry.name)
        {
            return Err(format!(
                "invalid tool manifest addition identity in {collection}"
            ));
        }
        manifest.insert(collection.to_string(), Value::Array(values));
    }
    Ok(Value::Object(manifest))
}

