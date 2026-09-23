//! Lossless diagnostic identities and bounded assertion details; raw logs stay on disk.
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;

pub fn inventory(output: &str) -> Value {
    let mut diagnostics = Vec::new();
    let mut failed = BTreeSet::new();
    let mut passed = BTreeSet::new();
    let mut assertions = Vec::new();
    let mut totals = Vec::new();
    let mut artifact_count = 0usize;
    let mut test_executables = BTreeSet::new();
    let lines = output.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            match record["reason"].as_str() {
                Some("compiler-message") => diagnostics.push(json!({
                    "package_id":record["package_id"],"level":record["message"]["level"],
                    "code":record["message"]["code"],"message":record["message"]["message"],
                    "spans":record["message"]["spans"],"children":record["message"]["children"],
                    "raw_line":index+1})),
                Some("compiler-artifact") => {
                    artifact_count += 1;
                    if record["profile"]["test"] == true
                        && let Some(executable) = record["executable"].as_str()
                    {
                        test_executables.insert(executable.to_owned());
                    }
                }
                _ => {}
            }
        }
        if let Some(rest) = line.strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                failed.insert(name.to_owned());
            }
            if let Some(name) = rest.strip_suffix(" ... ok") {
                passed.insert(name.to_owned());
            }
        }
        if line.starts_with("test result:") {
            totals.push(*line);
        }
        if line.contains("panicked at")
            || line.contains("assertion `")
            || line.trim_start().starts_with("left:")
            || line.trim_start().starts_with("right:")
        {
            let text = lines[index..(index + 5).min(lines.len())].join("\n");
            let end = text.char_indices().nth(4096).map_or(text.len(), |(i, _)| i);
            assertions.push(
                json!({"raw_start_line":index+1,"text":&text[..end],"complete":end==text.len()}),
            );
        }
    }
    json!({"diagnostics":diagnostics,"failed_tests":failed,"passed_tests":passed,
        "assertions":assertions,"test_summaries":totals,"artifact_count":artifact_count,"test_executables":test_executables,
        "failure_inventory_complete":true,"raw_line_count":lines.len()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_every_failure_and_assertion_amid_large_noise() {
        let mut log = "irrelevant detail\n".repeat(20000);
        for n in 0..1000 {
            log.push_str(&format!("{}\n",json!({"reason":"compiler-artifact","package_id":format!("dependency-{n}"),"filenames":["irrelevant.rlib"]})));
        }
        for n in 0..20 {
            log.push_str(&format!("test module::case_{n} ... FAILED\n"));
        }
        log.push_str("thread 'case' panicked at src/lib.rs:9:2:\nassertion `left == right` failed\n left: 1\nright: 2\ntest result: FAILED. 0 passed; 20 failed\n");
        let value = inventory(&log);
        assert_eq!(value["failed_tests"].as_array().unwrap().len(), 20);
        assert!(value["assertions"].to_string().contains("right: 2"));
        assert_eq!(value["test_summaries"].as_array().unwrap().len(), 1);
        assert_eq!(value["artifact_count"], 1000);
        assert!(
            value.to_string().len() < 8192,
            "dependency build chatter must not bury the complete failure inventory"
        );
    }
}
