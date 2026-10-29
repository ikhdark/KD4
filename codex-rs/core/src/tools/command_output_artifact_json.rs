//! Request-targeted lexical addresses. Unselected values are validated by the
//! borrowed RawValue parser, but never expand into a document-wide address map.
use super::*;
use serde::de::Error as _;
use serde::Deserializer as _;

const MAX_POINTER_ENTRIES: usize = 4_096;
const MAX_POINTER_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn index(bytes: &[u8], targets: &[String]) -> Option<RawJsonPointerIndex> {
    index_with_limits(bytes, targets, MAX_POINTER_ENTRIES, MAX_POINTER_BYTES)
}

fn index_with_limits(
    bytes: &[u8], targets: &[String], max_entries: usize, max_bytes: usize,
) -> Option<RawJsonPointerIndex> {
    fn needed(pointer: &str, targets: &[String]) -> bool {
        targets.iter().any(|target| {
            target == pointer
                || target.strip_prefix(pointer).is_some_and(|tail| tail.starts_with('/'))
                || pointer.strip_prefix(target).is_some_and(|tail| tail.starts_with('/'))
        })
    }
    struct Budget { entries: usize, bytes: usize, max_entries: usize, max_bytes: usize }
    impl Budget {
        fn charge(&mut self, pointer: &str) -> Option<()> {
            self.entries = self.entries.checked_add(1)?;
            self.bytes = self.bytes.checked_add(pointer.len().saturating_mul(3).saturating_add(256))?;
            (self.entries <= self.max_entries && self.bytes <= self.max_bytes).then_some(())
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
    fn visit(
        raw: &serde_json::value::RawValue, base: usize, pointer: String, depth: usize,
        targets: &[String], index: &mut RawJsonPointerIndex, budget: &mut Budget,
    ) -> Option<()> {
        if depth > 128 { return None; }
        budget.charge(&pointer)?;
        let text = raw.get();
        let children = if text.starts_with(['{', '[']) {
            serde_json::Deserializer::from_str(text).deserialize_any(Children {
                pointer: &pointer, targets, base, depth, index, budget,
            }).ok()?
        } else { Vec::new() };
        let start = text.as_ptr() as usize - base;
        index.pointers.insert(pointer, CanonicalJsonPointer {
            range: CanonicalByteRange::new(start as u64, (start + text.len()) as u64),
            exact_bytes: text.len() as u64, direct_children: children, recovery_chunk_bytes: None,
        });
        Some(())
    }
    let raw: &serde_json::value::RawValue = serde_json::from_slice(bytes).ok()?;
    let mut index = RawJsonPointerIndex { pointers: BTreeMap::new(), ambiguous: BTreeSet::new() };
    visit(raw, bytes.as_ptr() as usize, String::new(), 0, targets, &mut index,
        &mut Budget { entries: 0, bytes: 0, max_entries, max_bytes })?;
    Some(index)
}

/// A rejected large target must not invalidate an independently addressable
/// sibling. Commit only successful target indexes, under the same aggregate
/// address limits; failed traversals never retain their partially built map.
pub(super) fn index_preserving_siblings(
    bytes: &[u8], targets: &[String],
) -> Option<(RawJsonPointerIndex, BTreeSet<String>)> {
    let mut combined = RawJsonPointerIndex { pointers: BTreeMap::new(), ambiguous: BTreeSet::new() };
    let mut failed = BTreeSet::new();
    // Index each distinct target once, in a stable order: a repeated target
    // would charge the shared budget again and could report an indexed target
    // as failed.
    let unique_targets = targets.iter().collect::<BTreeSet<_>>();
    for target in unique_targets {
        let used = combined.pointers.keys().fold(0usize, |cost, pointer|
            cost.saturating_add(pointer.len().saturating_mul(3).saturating_add(256)));
        let Some(mut selected) = index_with_limits(
            bytes, std::slice::from_ref(target),
            MAX_POINTER_ENTRIES.saturating_sub(combined.pointers.len()),
            MAX_POINTER_BYTES.saturating_sub(used),
        ) else {
            failed.insert((*target).clone());
            continue;
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
