use anyhow::Context;
use anyhow::ensure;
use fs2::FileExt;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

pub fn update(
    directory: &Path,
    id: &str,
    root: &Path,
    consumers: Vec<Value>,
    reviewed: &BTreeSet<String>,
    compiler_verified: bool,
) -> anyhow::Result<Value> {
    ensure!(
        !id.is_empty()
            && id.len() <= 100
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "migration_id must be a bounded identifier"
    );
    std::fs::create_dir_all(directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("{id}.lock")))?;
    lock.lock_exclusive()?;
    let path = directory.join(format!("{id}.json"));
    let mut worklist = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            json!({"repository":root,"consumers":[],"reviewed":[],"review_revisions":{}})
        }
        Err(e) => return Err(e.into()),
    };
    ensure!(
        worklist["repository"] == json!(root),
        "migration belongs to another repository"
    );
    let retained = worklist["consumers"]
        .as_array_mut()
        .context("migration consumers")?;
    for consumer in consumers {
        if !retained.iter().any(|old| old["id"] == consumer["id"]) {
            retained.push(consumer);
        }
    }
    let known = worklist["consumers"]
        .as_array()
        .context("migration consumers")?
        .iter()
        .filter_map(|c| c["id"].as_str())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    ensure!(
        reviewed.is_subset(&known),
        "reviewed consumer is not in the retained migration"
    );
    let mut revisions = worklist
        .get("review_revisions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut accepted = BTreeSet::new();
    for consumer in worklist["consumers"].as_array().context("consumers")? {
        let key = consumer["id"].as_str().context("consumer id")?;
        let path = consumer["source"]["path"].as_str();
        let current = path.and_then(|path| match std::fs::read(path) {
            Ok(bytes) => Some(format!("{:x}", Sha256::digest(bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some("removed".into()),
            Err(_) => None,
        });
        if let Some(current) = current {
            if reviewed.contains(key) {
                revisions.insert(key.into(), json!(current));
            }
            if revisions.get(key) == Some(&json!(current)) {
                accepted.insert(key.to_owned());
            }
        }
    }
    worklist["review_revisions"] = json!(revisions);
    worklist["reviewed"] = json!(accepted);
    worklist["unresolved_consumers"] = json!(known.difference(&accepted).collect::<Vec<_>>());
    worklist["compiler_verified"] = json!(compiler_verified);
    // Explicit reviews describe disposition of the old consumers, while the
    // compiler checks the current representation and all selected targets.
    worklist["complete"] = json!(
        !known.is_empty()
            && known == accepted
            && compiler_verified
            && worklist["consumers"]
                .as_array()
                .is_some_and(|cs| cs.iter().all(|c| c["source"]["complete"] == true))
    );
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.write_all(&serde_json::to_vec(&worklist)?)?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(worklist)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_requires_every_consumer_and_current_compiler_success() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "fn a() {}").unwrap();
        std::fs::write(&b, "fn b() {}").unwrap();
        let consumers = vec![
            json!({"id":"a","source":{"path":a,"complete":true}}),
            json!({"id":"b","source":{"path":b,"complete":true}}),
        ];
        let first = update(
            dir.path(),
            "change",
            Path::new("repo"),
            consumers,
            &BTreeSet::from(["a".into()]),
            true,
        )
        .unwrap();
        assert_eq!(first["complete"], false);
        assert_eq!(first["unresolved_consumers"], json!(["b"]));
        let second = update(
            dir.path(),
            "change",
            Path::new("repo"),
            vec![],
            &BTreeSet::from(["b".into()]),
            false,
        )
        .unwrap();
        assert_eq!(second["complete"], false);
        assert_eq!(
            update(
                dir.path(),
                "change",
                Path::new("repo"),
                vec![],
                &BTreeSet::new(),
                true
            )
            .unwrap()["complete"],
            true
        );
        std::fs::write(&a, "fn a() { changed(); }").unwrap();
        let stale = update(
            dir.path(),
            "change",
            Path::new("repo"),
            vec![],
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(stale["complete"], false);
        assert_eq!(stale["unresolved_consumers"], json!(["a"]));
        assert!(
            update(
                dir.path(),
                "change",
                Path::new("elsewhere"),
                vec![],
                &BTreeSet::new(),
                true
            )
            .is_err()
        );
    }
}
