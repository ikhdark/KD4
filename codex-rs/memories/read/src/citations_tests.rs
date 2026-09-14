use super::parse_memory_citation;
use super::thread_ids_from_memory_citation;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn rejects_invalid_entries_and_preserves_valid_windows_locations() {
    let parsed = parse_memory_citation(vec![
        r"<citation_entries>
:1-2|note=[empty path]
   :1-2|note=[blank path]
MEMORY.md:9-2|note=[reversed]
MEMORY.md:x-2|note=[invalid number]
MEMORY.md:1-2|note=[missing bracket
C:\memories\MEMORY.md:2-4|note=[ valid ]
</citation_entries>"
            .to_string(),
    ])
    .unwrap();
    assert_eq!(parsed.entries.len(), 1);
    let entry = &parsed.entries[0];
    assert_eq!(
        (&*entry.path, entry.line_start, entry.line_end, &*entry.note),
        (r"C:\memories\MEMORY.md", 2, 4, "valid")
    );
    assert!(
        parse_memory_citation(vec![
            "<citation_entries>:3-1|note=[bad]</citation_entries>".into()
        ])
        .is_none()
    );
}

#[test]
fn deduplicates_rollout_ids_across_citation_blocks_in_encounter_order() {
    let first = ThreadId::new();
    let second = ThreadId::new();
    let parsed = parse_memory_citation(vec![
        format!("<rollout_ids>{first}</rollout_ids>"),
        format!("<thread_ids>{second}\n{first}</thread_ids>"),
    ])
    .unwrap();
    assert_eq!(
        parsed.rollout_ids,
        vec![first.to_string(), second.to_string()]
    );
    assert_eq!(
        thread_ids_from_memory_citation(&parsed),
        vec![first, second]
    );
}

#[test]
fn parse_memory_citation_supports_legacy_thread_ids() {
    let first = ThreadId::new();
    let second = ThreadId::new();

    let citations = vec![format!(
        "<memory_citation>\n<citation_entries>\nMEMORY.md:1-2|note=[x]\n</citation_entries>\n<thread_ids>\n{first}\nnot-a-uuid\n{second}\n</thread_ids>\n</memory_citation>"
    )];

    let parsed = parse_memory_citation(citations).expect("memory citation should parse");

    assert_eq!(
        thread_ids_from_memory_citation(&parsed),
        vec![first, second]
    );
}

#[test]
fn parse_memory_citation_supports_rollout_ids() {
    let thread_id = ThreadId::new();

    let citations = vec![format!(
        "<memory_citation>\n<rollout_ids>\n{thread_id}\n</rollout_ids>\n</memory_citation>"
    )];

    let parsed = parse_memory_citation(citations).expect("memory citation should parse");

    assert_eq!(thread_ids_from_memory_citation(&parsed), vec![thread_id]);
}

#[test]
fn parse_memory_citation_extracts_entries_and_rollout_ids() {
    let first = ThreadId::new();
    let second = ThreadId::new();
    let citations = vec![format!(
        "<citation_entries>\nMEMORY.md:1-2|note=[summary]\nrollout_summaries/foo.md:10-12|note=[details]\n</citation_entries>\n<rollout_ids>\n{first}\n{second}\n{first}\n</rollout_ids>"
    )];

    let parsed = parse_memory_citation(citations).expect("memory citation should parse");

    assert_eq!(
        parsed
            .entries
            .iter()
            .map(|entry| (
                entry.path.clone(),
                entry.line_start,
                entry.line_end,
                entry.note.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("MEMORY.md".to_string(), 1, 2, "summary".to_string()),
            (
                "rollout_summaries/foo.md".to_string(),
                10,
                12,
                "details".to_string()
            ),
        ]
    );
    assert_eq!(
        parsed.rollout_ids,
        vec![first.to_string(), second.to_string()]
    );
}
