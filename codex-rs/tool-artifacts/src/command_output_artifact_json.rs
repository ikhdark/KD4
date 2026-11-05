//! Request-targeted lexical addresses. Unselected values are validated by the
//! borrowed RawValue parser, but never expand into a document-wide address map.
use super::*;
use serde::de::Error as _;
use serde::Deserializer as _;

const MAX_POINTER_ENTRIES: usize = 4_096;
const MAX_POINTER_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) enum IndexError {
    Parser(String),
    DepthLimit,
    AddressBudget,
}

impl IndexError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::Parser(error) => format!("JSON parser rejected the snapshot: {error}; recover bytes instead."),
            Self::DepthLimit => "JSON address traversal exceeded depth 128; recover bytes instead.".into(),
            Self::AddressBudget => format!("JSON address index exceeded its {MAX_POINTER_ENTRIES}-entry or {MAX_POINTER_BYTES}-byte budget; recover bytes or select a narrower pointer."),
        }
    }
}

pub(super) fn index(bytes: &[u8], targets: &[String]) -> Result<RawJsonPointerIndex, IndexError> {
    index_with_limits(bytes, targets, MAX_POINTER_ENTRIES, MAX_POINTER_BYTES)
}

fn index_with_limits(
    bytes: &[u8], targets: &[String], max_entries: usize, max_bytes: usize,
) -> Result<RawJsonPointerIndex, IndexError> {
    fn needed(pointer: &str, targets: &[String]) -> bool {
        targets.iter().any(|target| {
            target == pointer
                || target.strip_prefix(pointer).is_some_and(|tail| tail.starts_with('/'))
                || pointer.strip_prefix(target).is_some_and(|tail| tail.starts_with('/'))
        })
    }
    struct Budget { entries: usize, bytes: usize, max_entries: usize, max_bytes: usize, failure: Option<IndexError> }
    impl Budget {
        fn charge(&mut self, pointer: &str) -> Option<()> {
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(pointer.len().saturating_mul(3).saturating_add(256));
            if self.entries > self.max_entries || self.bytes > self.max_bytes {
                self.failure = Some(IndexError::AddressBudget);
                return None;
            }
            Some(())
        }
    }
    struct Children<'a, 'i> {
        pointer: &'a str,
        targets: &'a [String],
        base: usize,
        depth: usize,
        index: &'i mut RawJsonPointerIndex,
        budget: &'i mut Budget,
    }
    impl<'de> serde::de::Visitor<'de> for Children<'_, '_> {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a lexical JSON container")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut children = Vec::new();
            let mut position = 0usize;
            while let Some(value) = sequence.next_element::<&'de serde_json::value::RawValue>()? {
                let child = format!("{}/{position}", self.pointer);
                if needed(&child, self.targets) {
                    visit(value, self.base, child.clone(), self.depth + 1, self.targets, self.index, self.budget)
                        .ok_or_else(|| A::Error::custom("bounded JSON address index exhausted"))?;
                    children.push(child);
                }
                position += 1;
            }
            Ok(children)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut children = BTreeSet::new();
            while let Some((key, value)) = map.next_entry::<String, &'de serde_json::value::RawValue>()? {
                let child = format!("{}/{}", self.pointer, key.replace('~', "~0").replace('/', "~1"));
                if !needed(&child, self.targets) { continue; }
                if !children.insert(child.clone()) {
                    self.index.ambiguous.insert(child);
                } else {
                    visit(value, self.base, child, self.depth + 1, self.targets, self.index, self.budget)
                        .ok_or_else(|| A::Error::custom("bounded JSON address index exhausted"))?;
                }
            }
            Ok(children.into_iter().collect())
        }
    }
    /// A pass over a subtree inside a selected value. Nothing in this request
    /// addresses its members, so they cost no index entries; a repeated decoded
    /// name anywhere in it still makes the selected value ambiguous.
    struct RepeatedNames<'b> {
        depth: usize,
        budget: &'b mut Budget,
    }
    impl<'de> serde::de::Visitor<'de> for RepeatedNames<'_> {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a lexical JSON container")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<bool, A::Error> {
            let mut repeated = false;
            while let Some(value) = sequence.next_element::<&'de serde_json::value::RawValue>()? {
                repeated |= repeated_names(value, self.depth + 1, self.budget)
                    .ok_or_else(|| A::Error::custom("bounded JSON address index exhausted"))?;
            }
            Ok(repeated)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let mut names = BTreeSet::new();
            let mut repeated = false;
            while let Some((name, value)) = map.next_entry::<String, &'de serde_json::value::RawValue>()? {
                repeated |= !names.insert(name);
                repeated |= repeated_names(value, self.depth + 1, self.budget)
                    .ok_or_else(|| A::Error::custom("bounded JSON address index exhausted"))?;
            }
            Ok(repeated)
        }
    }
    fn repeated_names(raw: &serde_json::value::RawValue, depth: usize, budget: &mut Budget) -> Option<bool> {
        if depth > 128 {
            budget.failure = Some(IndexError::DepthLimit);
            return None;
        }
        let text = raw.get();
        if !text.starts_with(['{', '[']) {
            return Some(false);
        }
        match serde_json::Deserializer::from_str(text).deserialize_any(RepeatedNames { depth, budget }) {
            Ok(repeated) => Some(repeated),
            Err(error) => {
                if budget.failure.is_none() {
                    budget.failure = Some(IndexError::Parser(error.to_string()));
                }
                None
            }
        }
    }
    fn visit(
        raw: &serde_json::value::RawValue, base: usize, pointer: String, depth: usize,
        targets: &[String], index: &mut RawJsonPointerIndex, budget: &mut Budget,
    ) -> Option<()> {
        if depth > 128 {
            budget.failure = Some(IndexError::DepthLimit);
            return None;
        }
        budget.charge(&pointer)?;
        // A direct member of a selected value is listed by its parent. A later
        // selection of it indexes its own members, so its subtree needs no entries.
        let addressed = targets.iter().any(|target| {
            target == &pointer
                || target.strip_prefix(pointer.as_str()).is_some_and(|tail| tail.starts_with('/'))
        });
        if !addressed {
            if repeated_names(raw, depth, budget)? {
                index.ambiguous.insert(pointer);
            }
            return Some(());
        }
        let text = raw.get();
        let children = if text.starts_with(['{', '[']) {
            match serde_json::Deserializer::from_str(text).deserialize_any(Children {
                pointer: &pointer, targets, base, depth, index, budget,
            }) {
                Ok(children) => children,
                Err(error) => {
                    if budget.failure.is_none() {
                        budget.failure = Some(IndexError::Parser(error.to_string()));
                    }
                    return None;
                }
            }
        } else { Vec::new() };
        let start = text.as_ptr() as usize - base;
        index.pointers.insert(pointer, CanonicalJsonPointer {
            range: CanonicalByteRange::new(start as u64, (start + text.len()) as u64),
            exact_bytes: text.len() as u64, direct_children: children, recovery_chunk_bytes: None,
        });
        Some(())
    }
    let raw: &serde_json::value::RawValue = serde_json::from_slice(bytes)
        .map_err(|error| IndexError::Parser(error.to_string()))?;
    let mut index = RawJsonPointerIndex { pointers: BTreeMap::new(), ambiguous: BTreeSet::new() };
    let mut budget = Budget { entries: 0, bytes: 0, max_entries, max_bytes, failure: None };
    if visit(raw, bytes.as_ptr() as usize, String::new(), 0, targets, &mut index, &mut budget).is_none() {
        return Err(budget.failure.unwrap_or(IndexError::AddressBudget));
    }
    Ok(index)
}

/// A rejected large target must not invalidate an independently addressable
/// sibling. Commit only successful target indexes, under the same aggregate
/// address limits; failed traversals never retain their partially built map.
pub(super) fn index_preserving_siblings(
    bytes: &[u8], targets: &[String],
) -> Option<(RawJsonPointerIndex, BTreeMap<String, IndexError>)> {
    let mut combined = RawJsonPointerIndex { pointers: BTreeMap::new(), ambiguous: BTreeSet::new() };
    let mut failed = BTreeMap::new();
    // Index each distinct target once, in a stable order: a repeated target
    // would charge the shared budget again and could report an indexed target
    // as failed.
    let unique_targets = targets.iter().collect::<BTreeSet<_>>();
    for target in unique_targets {
        let used = combined.pointers.keys().fold(0usize, |cost, pointer|
            cost.saturating_add(pointer.len().saturating_mul(3).saturating_add(256)));
        let mut selected = match index_with_limits(
            bytes, std::slice::from_ref(target),
            MAX_POINTER_ENTRIES.saturating_sub(combined.pointers.len()),
            MAX_POINTER_BYTES.saturating_sub(used),
        ) {
            Ok(selected) => selected,
            Err(error) => {
                failed.insert((*target).clone(), error);
                continue;
            }
        };
        for (pointer, entry) in selected.pointers {
            if let Some(existing) = combined.pointers.get_mut(&pointer) {
                existing.direct_children.extend(entry.direct_children);
                existing.direct_children.sort();
                existing.direct_children.dedup();
            } else {
                combined.pointers.insert(pointer, entry);
            }
        }
        combined.ambiguous.append(&mut selected.ambiguous);
    }
    (!combined.pointers.is_empty()).then_some((combined, failed))
}
