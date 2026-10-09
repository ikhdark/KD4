//! Complete core turns; the provider is scripted, not live inference.
use super::assert_eq;
use super::*;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;

const BEHAVIORAL_VALIDATOR: &str = "import sys,time; from pathlib import Path; from changed_fixture import transform; assert transform(41) == 42; assert transform(-1) == 0; time.sleep(float(sys.argv[1])); sys.stdout.write(Path('answer.txt').read_text(encoding='utf-8'))";

#[test_case::test_case("delivery")]
#[cfg_attr(windows, test_case::test_case("leaf-glob"))]
#[test_case::test_case("lookup")]
#[test_case::test_case("batch")]
#[test_case::test_case("read-helper")]
#[test_case::test_case("full-recovery-helper")]
#[test_case::test_case("validation-helper")]
#[test_case::test_case("retained-batch")]
#[test_case::test_case("bounded-inventory")]
#[test_case::test_case("multi-range")]
#[test_case::test_case("search-inspect")]
#[test_case::test_case("full-file")]
#[test_case::test_case("graph")]
#[test_case::test_case("recovery")]
#[test_case::test_case("large-recovery")]
#[test_case::test_case("recipe-recovery")]
#[test_case::test_case("poll")]
#[test_case::test_case("retained")]
#[test_case::test_case("validation_overlap")]
#[test_case::test_case("validation-progress")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delivery_preserves_answer_and_removes_final_model_request(scenario: &str) -> Result<()> {
    require_network!();
    for candidate in [false, true] {
        eprintln!("direct-delivery scenario={scenario} candidate={candidate}");
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
            config.background_terminal_max_timeout = 5_000;
        });
        let test = builder.build(&server).await?;
        let answer = "Verified answer: α → β.\nPreserve this exact text.\n";
        fs::write(test.cwd.path().join("answer.txt"), answer)?;
        if scenario == "validation-helper" {
            fs::write(test.cwd.path().join("changed_fixture.py"), "def transform(value):\n    return value + 1\n")?;
        }
        fs::write(test.cwd.path().join("first.txt"), "Verified answer: α → β.\n")?;
        fs::write(test.cwd.path().join("second.txt"), "Preserve this exact text.\n")?;
        fs::write(test.cwd.path().join("ranges.txt"), "Verified answer: α → β.\nnot requested\nPreserve this exact text.\n")?;
        let mut inventory = (0..128).map(|index| serde_json::json!({
            "path":format!("unrelated/{index}.txt"), "selected":false,
        })).collect::<Vec<_>>();
        inventory.extend([
            serde_json::json!({"path":"first.txt","selected":true}),
            serde_json::json!({"path":"second.txt","selected":true}),
        ]);
        fs::write(test.cwd.path().join("inventory.json"), serde_json::to_vec(&inventory)?)?;
        let bulk = serde_json::json!({"answer":answer,"padding":"filler ".repeat(match scenario {
            "recipe-recovery" => 160_000, "large-recovery" => 10_000, _ => 900,
        })}).to_string();
        fs::write(test.cwd.path().join("bulk.json"), &bulk)?;
        // Larger than a model display page, but comfortably within the exact
        // script payload limit. A short computed answer must not force paging.
        let large = serde_json::json!({"answer":answer,"padding":"λ 日本語\r\n".repeat(10_000)}).to_string();
        fs::write(test.cwd.path().join("large.json"), &large)?;
        if scenario == "full-recovery-helper" {
            let large = serde_json::json!({"answer":answer,"padding":"λ 日本語\r\n".repeat(100_000)}).to_string();
            fs::write(test.cwd.path().join("recovery.json"), large)?;
        }
        let read_answer = "const r = await tools.read_file({path:'answer.txt'}); if (!r.file_complete) throw new Error('incomplete source'); text(r.results[0].text);";
        let (prepare, compute) = match scenario {
            "delivery" => (None, read_answer.to_string()),
            "leaf-glob" => {
                let rg = which::which("rg")?;
                let discover = serde_json::json!({
                    "program":rg, "args":["--files", "--glob", "answer*.txt", "."],
                });
                let search = serde_json::json!({
                    "program":rg,
                    "args":["--no-heading", "--color", "never", ".", if candidate { "answer*.txt" } else { "answer.txt" }],
                });
                (
                    Some(format!("const found = await tools.exec_command({discover}); if (!found.process_exited || found.exit_code !== 0) throw Error('discovery failed'); text(found.output);")),
                    format!("const result = await tools.exec_command({search}); if (!result.process_exited || result.exit_code !== 0 || result.output_reduced) throw Error('search incomplete'); text(result.output);"),
                )
            },
            "lookup" => (
                Some("text(resolve_tool('read_file').description);".to_string()),
                "const read = resolve_tool('read_file'); if (!read.description.includes('UTF-8')) throw Error('missing contract'); const r = await read({path:'answer.txt'}); if (!r.file_complete) throw Error('incomplete source'); text(r.results[0].text);".to_string(),
            ),
            "batch" | "read-helper" => (
                Some("const r = await tools.read_file({path:'first.txt'}); if (!r.file_complete) throw Error('incomplete first file'); store('first', r.results[0].text); text('first file read');".to_string()),
                if candidate && scenario == "read-helper" {
                    "const rows = await read_files(['first.txt','second.txt','first.txt']); store('reads',rows); if (rows.some(r => r.status !== 'fulfilled' || !r.value.file_complete) || rows[0].value !== rows[2].value) throw Error('incomplete/different batch'); text(rows[0].value.initial.results[0].text + rows[1].value.initial.results[0].text);".to_string()
                } else if candidate {
                    "const results = await Promise.allSettled(['first.txt','second.txt'].map(path => tools.read_file({path}))); for (const r of results) if (r.status !== 'fulfilled' || !r.value.file_complete) throw Error('incomplete batch'); text(results.map(r => r.value.results[0].text).join(''));".to_string()
                } else {
                    "const r = await tools.read_file({path:'second.txt'}); if (!r.file_complete) throw Error('incomplete second file'); text(load('first') + r.results[0].text);".to_string()
                },
            ),
            "retained-batch" => (
                Some(format!(r#"const batch = await Promise.allSettled(['first.txt','second.txt'].map(path => tools.read_file({{path}})));
                    store('batch', {});
                    for (const r of batch) if (r.status !== 'fulfilled' || !r.value.complete || !r.value.file_complete) throw Error('incomplete batch');"#,
                    if candidate { "batch" } else { "[batch[0]]" })),
                if candidate {
                    "const retained = load('batch'); if (retained.length !== 2) throw Error('lost unprinted sibling'); text(retained.map(r => r.value.results[0].text).join(''));".to_string()
                } else {
                    "const second = await tools.read_file({path:'second.txt'}); if (!second.complete || !second.file_complete) throw Error('missing second file'); text(load('batch')[0].value.results[0].text + second.results[0].text);".to_string()
                },
            ),
            "bounded-inventory" => (
                Some("const inventory = await tools.read_file({path:'inventory.json'}); if (!inventory.complete || !inventory.file_complete) throw Error('incomplete inventory'); const rows = JSON.parse(inventory.results[0].text); if (rows.length !== 130) throw Error('lost inventory rows'); store('inventory', rows);".to_string()),
                "const selected = load('inventory').filter(row => row.selected); if (selected.length !== 2) throw Error('wrong scope'); const results = await Promise.allSettled(selected.map(row => tools.read_file({path:row.path}))); for (const r of results) if (r.status !== 'fulfilled' || !r.value.file_complete) throw Error('missing selected source'); text(results.map(r => r.value.results[0].text).join(''));".to_string(),
            ),
            "full-file" => (
                Some("const r = await tools.read_file({path:'large.json', selectors:[{kind:'bytes',start:0,end:1}]}); if (!r.complete || r.file_complete) throw Error('expected exact partial source'); store('largeHead', r);".to_string()),
                if candidate {
                    "const r = await tools.read_file({path:'large.json'}); if (!r.complete || !r.file_complete) throw Error('full source required'); const v = JSON.parse(r.results[0].text); if (v.padding !== 'λ 日本語\\r\\n'.repeat(10000)) throw Error('source corruption'); text(v.answer);".to_string()
                } else {
                    format!("const r = await tools.read_file({{path:'large.json',selectors:[{{kind:'bytes',start:1,end:{}}}]}}); const head = load('largeHead'); if (!r.complete || !head.source_sha256 || r.source_sha256 !== head.source_sha256) throw Error('incomplete or changed source'); const v = JSON.parse(head.results[0].text + r.results[0].text); if (v.padding !== 'λ 日本語\\r\\n'.repeat(10000)) throw Error('source corruption'); text(v.answer);", large.len())
                },
            ),
            "full-recovery-helper" => (
                Some("const head = await tools.read_file({path:'recovery.json'}); if (!head.complete || head.file_complete) throw Error('expected partial source'); store('head',head);".to_string()),
                if candidate {
                    "const rows = await read_files(['recovery.json'], {full:true}); store('reads',rows); if (rows[0].status !== 'fulfilled' || !rows[0].value.file_complete) throw Error(JSON.stringify(rows)); const e = rows[0].value; const value = JSON.parse([e.initial,...e.pages].flatMap(p => p.results).map(p => p.text).join('')); if(value.padding !== 'λ 日本語\\r\\n'.repeat(100000)) throw Error('source corruption'); text(value.answer);".to_string()
                } else {
                    "const head=load('head'); let parts=[head.results[0].text], offset=head.continuation.start; while(offset<head.canonical_bytes) {const end=head.canonical_bytes; const r=await tools.read_tool_output({artifact_id:head.artifact_id,selectors:[{kind:'bytes',start:offset,end}],max_bytes:1048576}); if(r.canonical_sha256!==head.source_sha256 || r.canonical_bytes!==head.canonical_bytes) throw Error('snapshot changed'); const before=offset; for(const p of r.results){if(p.status==='selector_too_large' && p.complete===false && p.text===undefined && Array.isArray(p.child_selectors)) continue; if(p.status!=='ok'||p.complete!==true||p.canonical_range.start!==offset||p.canonical_range.end>end||p.canonical_range.end<=offset||typeof p.text!=='string') throw Error('missing source'); parts.push(p.text); offset=p.canonical_range.end;} if(offset<=before) throw Error('no progress'); if(offset<end && (r.continuation_stop?.reason!=='budget' || r.continuation_stop.resumable!==true || r.continuation_stop.selector?.start!==offset || r.continuation_stop.selector.end!==end)) throw Error('unproven continuation'); if(offset===end && !r.complete) throw Error('incomplete source');} const value=JSON.parse(parts.join('')); if(value.padding!=='λ 日本語\\r\\n'.repeat(100000)) throw Error('source corruption'); text(value.answer);".to_string()
                },
            ),
            "multi-range" => (
                Some("const r = await tools.read_file({path:'ranges.txt',selectors:[{kind:'lines',start:1,end:1}]}); if (!r.complete) throw Error('incomplete first range'); store('firstRange', r);".to_string()),
                if candidate {
                    "const r = await tools.read_file({path:'ranges.txt',selectors:[{kind:'lines',start:1,end:1},{kind:'lines',start:3,end:3}]}); if (!r.complete || r.file_complete || r.results.length !== 2) throw Error('wrong range coverage'); text(r.results.map(x => x.text).join(''));".to_string()
                } else {
                    "const r = await tools.read_file({path:'ranges.txt',selectors:[{kind:'lines',start:3,end:3}]}); const first = load('firstRange'); if (!r.complete || r.source_sha256 !== first.source_sha256) throw Error('changed range source'); text(first.results[0].text + r.results[0].text);".to_string()
                },
            ),
            "search-inspect" => (
                Some("const r = await tools.read_file({path:'answer.txt',selectors:[{kind:'search',query:'Verified',context_lines:1}]}); if (!r.complete || !r.file_complete) throw Error('incomplete search evidence'); store('search', r);".to_string()),
                if candidate {
                    "const r = await tools.read_file({path:'answer.txt',selectors:[{kind:'search',query:'Verified',context_lines:1}]}); if (!r.complete || !r.file_complete) throw Error('incomplete search evidence'); text(r.results[0].value.hydrated_ranges.map(x => x.text).join(''));".to_string()
                } else {
                    "const search = load('search'); const r = await tools.read_file({path:'answer.txt',selectors:search.results[0].value.hydrated_ranges.map(x => x.selector)}); if (!r.complete || !r.file_complete || r.source_sha256 !== search.source_sha256) throw Error('changed inspection source'); text(r.results.map(x => x.text).join(''));".to_string()
                },
            ),
            "graph" => (
                Some("const r = await tools.read_file({path:'first.txt'}); if (!r.file_complete) throw Error('incomplete prerequisite'); store('first', r.results[0].text);".to_string()),
                if candidate {
                    r#"const results = await run_graph([
                        {id:'first', requires:['read_file'], run:() => tools.read_file({path:'first.txt'}), accept:r => r.complete && r.file_complete},
                        {id:'second', requires:['read_file'], run:() => tools.read_file({path:'second.txt'}), accept:r => r.complete && r.file_complete},
                        {id:'answer', deps:['first','second'], run:r => r.first.results[0].text + r.second.results[0].text, accept:r => r.length > 0}
                    ], {concurrency:2}); text(results.answer.value);"#.to_string()
                } else {
                    "const r = await tools.read_file({path:'second.txt'}); if (!r.file_complete) throw Error('incomplete prerequisite'); text(load('first') + r.results[0].text);".to_string()
                },
            ),
            "recovery" => (
                Some("const head = await tools.read_file({path:'bulk.json', selectors:[{kind:'bytes',start:0,end:1}]}); store('artifact', head.artifact_id);".to_string()),
                format!("const r = await tools.read_tool_output({{artifact_id:load('artifact'),selectors:[{{kind:'bytes',start:0,end:{}}}]}}); if (!r.complete) throw Error('recovery required another handoff'); text(JSON.parse(r.results[0].text).answer);", bulk.len()),
            ),
            "large-recovery" => (
                Some("{ const head = await tools.read_file({path:'bulk.json',selectors:[{kind:'bytes',start:0,end:1}]}); if (!head.complete) throw Error('missing head'); store('artifactHead',head); }".to_string()),
                format!(r#"const head = load('artifactHead'); let offset = 1; let parts = [head.results[0].text];
                    let selectors = [{{kind:'bytes',start:1,end:{}}}]; let calls = 0;
                    while (selectors) {{
                        const r = await tools.read_tool_output({{artifact_id:head.artifact_id, selectors, max_bytes:{}}}); calls++;
                        if (r.canonical_sha256 !== head.source_sha256) throw Error('snapshot identity changed');
                        const before = offset;
                        for (const p of r.results) if (p.status === 'ok' && p.text !== undefined) {{
                            if (p.canonical_range.start !== offset) throw Error('noncontiguous recovery');
                            parts.push(p.text); offset = p.canonical_range.end;
                        }}
                        if (offset <= before) throw Error('recovery made no progress');
                        if (r.complete) selectors = null;
                        else {{
                            if (r.continuation_stop?.reason !== 'budget' || !r.continuation_stop.resumable) throw Error('unrecoverable');
                            selectors = [r.continuation_stop.selector];
                        }}
                    }}
                    if (offset !== {} || ({} && calls !== 1)) throw Error('incomplete or redundant recovery');
                    const value = JSON.parse(parts.join('')); if (value.padding !== 'filler '.repeat(10000)) throw Error('corrupt payload'); text(value.answer);"#,
                    bulk.len(), if candidate { 1024 * 1024 } else { 16_384 }, bulk.len(), candidate),
            ),
            "recipe-recovery" => (
                Some("{ const head = await tools.read_file({path:'bulk.json'}); if (!head.complete || head.file_complete || !head.recovery) throw Error('expected retained suffix'); store('recipeHead',head); }".to_string()),
                format!(r#"const head = load('recipeHead'); let args = {{...head.recovery.arguments}};
                    if (!{}) delete args.max_bytes;
                    let offset = head.continuation.start, parts = [head.results[0].text], calls = 0;
                    while (offset < head.canonical_bytes && calls < 64) {{
                        const r = await tools.read_tool_output(args); ++calls;
                        if (r.artifact_id !== head.artifact_id || r.canonical_sha256 !== head.source_sha256 ||
                            r.canonical_bytes !== head.canonical_bytes) throw Error('snapshot changed');
                        const before = offset;
                        for (const part of r.results) {{
                            if (part.status === 'selector_too_large' && !part.complete && part.text === undefined &&
                                (Array.isArray(part.child_selectors) || part.continuation?.kind === 'bytes')) continue;
                            if (part.status !== 'ok' || !part.complete || typeof part.text !== 'string' ||
                                part.canonical_range.start !== offset || part.canonical_range.end <= offset ||
                                part.canonical_range.end > head.canonical_bytes) throw Error('incomplete source: '+JSON.stringify({{status:part.status,complete:part.complete,range:part.canonical_range,continuation:part.continuation,offset,calls,stop:r.continuation_stop}}));
                            offset = part.canonical_range.end; parts.push(part.text);
                        }}
                        if (offset <= before) throw Error('no recovery progress');
                        if (r.complete) break;
                        const stop = r.continuation_stop;
                        if (stop?.reason !== 'budget' || !stop.resumable || stop.selector?.kind !== 'bytes' ||
                            stop.selector.start !== offset || stop.selector.end !== head.canonical_bytes) throw Error('recovery needs review');
                        args = {{...args,selectors:[stop.selector]}};
                    }}
                    if (offset !== head.canonical_bytes || ({} && calls !== 1)) throw Error('incomplete/redundant recovery');
                    const value = JSON.parse(parts.join(''));
                    if (value.padding !== 'filler '.repeat(160000)) throw Error('changed evidence');
                    text(value.answer);"#, candidate, candidate),
            ),
            "retained" => (
                Some("const r = await tools.read_file({path:'answer.txt'}); if (!r.file_complete) throw Error('incomplete source'); store('evidence', r);".to_string()),
                if candidate {
                    "const retained = load('evidence'); if (!retained.file_complete || !retained.source_sha256) throw Error('missing retained evidence'); text(retained.results[0].text);".to_string()
                } else {
                    "const r = await tools.read_file({path:'answer.txt',force_fresh:true}); if (!r.file_complete || r.source_sha256 !== load('evidence').source_sha256) throw Error('source changed'); text(r.results[0].text);".to_string()
                },
            ),
            "validation_overlap" | "validation-progress" | "validation-helper" => {
                let python = which::which("python").or_else(|_| which::which("python3"))?;
                let script = if scenario == "validation-helper" {
                    BEHAVIORAL_VALIDATOR
                } else if scenario == "validation-progress" {
                    "import sys,time; sys.stdout.buffer.write(sys.argv[1].encode('utf-8')); sys.stdout.buffer.flush(); time.sleep(3)"
                } else {
                    "import sys,time; time.sleep(3); sys.stdout.write(sys.argv[1])"
                };
                let command = serde_json::json!({
                    "program":python,
                    "args":["-X", "utf8", "-c", script, if scenario == "validation-helper" { "3" } else { answer }],
                    "yield_time_ms":250,
                });
                // The wait models read-only report work, not inference. No
                // mutation, resource conflict, or second validation is hidden.
                let review = "const source = await tools.read_file({path:'answer.txt'}); if (!source.file_complete) throw Error('incomplete review'); store('review', source.results[0].text); await new Promise(resolve => setTimeout(resolve, 1000));";
                let launch = format!("let r = await tools.exec_command({command});");
                let drain = if scenario == "validation-progress" && candidate {
                    "const completed = await await_command(r); const terminal = completed.terminal; if (!terminal.streams_complete || terminal.stdout !== load('review') || !completed.observations.some(p => !p.process_exited && p.output.includes(load('review')))) throw Error('incomplete terminal/progress evidence: '+JSON.stringify({terminal,observations:completed.observations,review:load('review')})); text(terminal.stdout);"
                } else if scenario == "validation-progress" {
                    "const observations = [r]; while (r.session_id && !r.process_exited) { r = await tools.write_stdin({session_id:r.session_id,incarnation:r.session_capabilities.incarnation,wait_for_output:true}); observations.push(r); } if (!r.process_exited || r.exit_code !== 0 || !r.streams_complete || r.stdout !== load('review') || !observations.some(p => !p.process_exited && p.output.includes(load('review')))) throw Error('incomplete terminal/progress evidence: '+JSON.stringify({terminal:r,observations,review:load('review')})); text(r.stdout);"
                } else if candidate && scenario == "validation-helper" {
                    "const completed = await await_command(r); const output = completed.observations.map(p => p.output).join(''); if (completed.observations.some(p => p.output_reduced) || output !== load('review')) throw Error('validation or review mismatch'); text(output);"
                } else {
                    "let output = r.output; while (r.session_id && !r.process_exited) { r = await tools.write_stdin({session_id:r.session_id,incarnation:r.session_capabilities.incarnation,wait_for_output:true}); output += r.output; } if (!r.process_exited || r.exit_code !== 0 || output !== load('review')) throw Error('validation or review mismatch'); text(output);"
                };
                (
                    Some(review.to_string()),
                    if candidate {
                        format!("{launch}\n{review}\n{drain}")
                    } else {
                        format!("{launch}\n{drain}")
                    },
                )
            }
            "poll" => {
                let python = which::which("python").or_else(|_| which::which("python3"))?;
                let command = serde_json::json!({
                    "program":python,
                    "args":["-X", "utf8", "-c", "import sys,time; time.sleep(8); sys.stdout.write(sys.argv[1])", answer],
                    "yield_time_ms":250,
                });
                (
                    Some(format!("const started = await tools.exec_command({command}); if (!started.session_id) throw Error('fixture must start a background process'); store('process', started);")),
                    // Output can precede process exit. Drain that mechanical
                    // transition in-cell, but reject every empty live result:
                    // those are the timer-only handoffs this regression targets.
                    "let output = ''; let r; do { r = await tools.write_stdin({session_id:load('process').session_id,incarnation:load('process').session_capabilities.incarnation}); if (!r.process_exited && !r.output) throw Error('timer-only handoff'); output += r.output; } while (!r.process_exited && r.session_id); if (!r.process_exited || r.exit_code !== 0) throw Error('command did not succeed'); text(output);".to_string(),
                )
            }
            _ => unreachable!(),
        };
        if !candidate && let Some(prepare) = &prepare {
            let extra = if scenario == "poll" {
                "const bounded = await tools.write_stdin({session_id:load('process').session_id,incarnation:load('process').session_capabilities.incarnation,wait_for_output:false}); if (bounded.process_exited) throw Error('fixture must cross a bounded poll'); text('still waiting');"
            } else if scenario == "bounded-inventory" {
                "text(load('inventory'));"
            } else {
                "text('prepared');"
            };
            responses::mount_sse_once(&server, sse(vec![
                ev_response_created("prepare"),
                ev_custom_tool_call("prepare", "exec", &format!("{prepare}\n{extra}")),
                ev_completed("prepare"),
            ])).await;
        }
        let prefix = if candidate {
            "// @exec: {\"deliver\": true,\"max_output_tokens\":64}\n"
        } else {
            ""
        };
        // Discovery and independent reads already have same-cell APIs. Recovery
        // and polling also need their owners not to manufacture a model boundary.
        let inline_prepare = if candidate && matches!(scenario, "recovery" | "large-recovery" | "recipe-recovery" | "poll" | "retained" | "retained-batch" | "bounded-inventory") {
            prepare.as_deref().unwrap_or_default()
        } else {
            ""
        };
        let code = format!("{prefix}{inline_prepare}\n{compute}");
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("initial"),
                ev_custom_tool_call("compute-answer", "exec", &code),
                ev_completed("initial"),
            ]),
        )
        .await;
        // Also mounted for the candidate: a regression finishes normally
        // and fails our request-count assertion rather than timing out.
        let final_response = responses::mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("answer", answer),
                ev_completed("final"),
            ]),
        )
        .await;
        let (sandbox_policy, permission_profile) =
            turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
        let started_at = std::time::Instant::now();
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: format!("Complete the {scenario} task and return the exact verified answer."),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            })
            .await?;
        let mut started = Vec::new();
        let mut messages = Vec::new();
        let completed = core_test_support::wait_for_event_with_timeout(
            &test.codex,
            |event| {
                match event {
                    EventMsg::ItemStarted(event) => {
                        if let TurnItem::AgentMessage(message) = &event.item {
                            started.push(message.id.clone());
                        }
                    }
                    EventMsg::ItemCompleted(event) => {
                        if let TurnItem::AgentMessage(message) = &event.item {
                            messages.push(message.clone());
                        }
                    }
                    _ => {}
                }
                matches!(event, EventMsg::TurnComplete(_))
            },
            Duration::from_secs(30),
        )
        .await;
        let EventMsg::TurnComplete(completed) = completed else {
            unreachable!()
        };
        assert!(completed.error.is_none(), "{:?}", completed.error);
        assert_eq!(completed.last_agent_message.as_deref(), Some(answer));
        assert_eq!(messages.len(), 1, "one visible answer before turn completion");
        assert_eq!(started, vec![messages[0].id.clone()]);
        assert!(matches!(
            messages[0].content.as_slice(),
            [AgentMessageContent::Text { text }] if text == answer
        ));
        if candidate {
            if messages[0].phase != Some(MessagePhase::FinalAnswer) {
                let request = final_response.single_request();
                let (output, success) = custom_tool_output_body_and_success(&request, "compute-answer");
                panic!("{scenario}: direct delivery fell back ({success:?}): {output}");
            }
        }
        test.codex.flush_rollout().await?;
        let (items, _, errors) = codex_core::RolloutRecorder::load_rollout_items(
            &test.codex.rollout_path().expect("rollout path"),
        )
        .await?;
        assert_eq!(errors, 0);
        let persisted = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(ResponseItem::Message {
                    id, role, content, ..
                }) if role == "assistant" => Some((id, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted.len(), 1, "the visible answer must survive reopening");
        assert_eq!(
            persisted[0].0.as_ref().map(|id| id.as_str()),
            Some(messages[0].id.as_str())
        );
        assert_eq!(
            persisted[0].1,
            &vec![ContentItem::OutputText {
                text: answer.to_string()
            }]
        );
        if candidate {
            let surfaced = completed.surfaced_result.as_ref().expect("direct answer");
            assert_eq!(surfaced.adapter, "code_mode_delivery");
            assert_eq!(surfaced.canonical_message.as_deref(), Some(answer));
        } else {
            assert!(completed.surfaced_result.is_none());
            let output = custom_tool_output_last_non_empty_text(
                &final_response.single_request(),
                "compute-answer",
            )
            .expect("computed answer");
            assert_eq!(output, answer);
        }
        let model_requests = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().contains("responses"))
            .count();
        assert_eq!(model_requests, if candidate { 1 } else { 2 + usize::from(prepare.is_some()) }, "{scenario}");
        let timing = completed.timing.as_ref().expect("turn timing");
        assert_eq!(timing.counters.logical_generation_count as usize, model_requests);
        assert_eq!(timing.counters.model_request_count as usize, model_requests);
        let timing_json = serde_json::to_value(timing)?;
        let reads = timing_json["toolCalls"].as_array().expect("tool timing").iter()
            .filter(|call| call["toolName"] == "read_file").count();
        if matches!(scenario, "retained" | "full-file" | "multi-range" | "search-inspect") {
            assert_eq!(reads, if candidate { 1 } else { 2 }, "no repeat read without drift");
        }
        if scenario == "retained-batch" {
            assert_eq!(reads, if candidate { 2 } else { 3 }, "unprinted siblings must survive the phase boundary");
        }
        if scenario == "bounded-inventory" {
            assert_eq!(reads, 3, "same complete inventory and selected-source coverage");
        }
        let calls = timing_json["toolCalls"].as_array().expect("tool timing");
        if scenario == "read-helper" {
            assert_eq!(reads, 2, "duplicate paths share one observation");
        }
        if scenario == "full-recovery-helper" {
            assert_eq!(reads, 1, "recovery never reopens the mutable source");
            assert!(calls.iter().any(|call| call["toolName"] == "read_tool_output"));
        }
        if scenario == "leaf-glob" {
            let commands = calls.iter().filter(|call| call["toolName"] == "exec_command").count();
            assert_eq!(commands, if candidate { 1 } else { 2 }, "no discovery-only command or retry");
        }
        if matches!(scenario, "large-recovery" | "recipe-recovery") {
            let recoveries = calls.iter().filter(|call| call["toolName"] == "read_tool_output").count();
            if candidate { assert_eq!(recoveries, 1); } else { assert!(recoveries > 1); }
        }
        let outer_calls = calls.iter().filter(|call| call["toolName"] == "exec").count();
        assert_eq!(outer_calls, if candidate { 1 } else { 1 + usize::from(prepare.is_some()) }, "{scenario}: outer tool dispatches");
        assert!(calls.iter().all(|call| call["outcome"] == "success"
            || (call["outcome"] == "yielded"
                && matches!(call["toolName"].as_str(), Some("exec_command" | "write_stdin")))),
            "{scenario}: completion hid a failed child: {calls:?}");
        eprintln!("LATENCY_COMPARISON {}", serde_json::json!({
            "scenario":scenario, "candidate":candidate,
            "wall_ms":started_at.elapsed().as_millis(),
            "model_requests":model_requests, "native_reads":reads,
            "native_recoveries":calls.iter().filter(|call| call["toolName"] == "read_tool_output").count(),
            "outer_tool_calls":outer_calls,
            "projected_tool_output_bytes":timing_json["counters"]["toolOutputModelByteCount"],
            "exact_answer_and_persistence_verified":true,
            "provider":"scripted",
        }));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delivery_accepts_schema_valid_json_and_supported_media() -> Result<()> {
    require_network!();
    for (code, expected, schema) in [
        ("text({answer: 42});", "{\"answer\":42}",
         Some(serde_json::json!({"type":"object","properties":{"answer":{"const":42}},"required":["answer"],"additionalProperties":false}))),
        ("image('DATA:image/png;base64,AAAA');",
         "![image](<DATA:image/png;base64,AAAA>)", None),
    ] {
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
        });
        let test = builder.build(&server).await?;
        responses::mount_sse_once(&server, sse(vec![
            ev_response_created("initial"),
            ev_custom_tool_call("delivery", "exec", &format!("// @exec: {{\"deliver\":true}}\n{code}")),
            ev_completed("initial"),
        ])).await;
        responses::mount_sse_once(&server, sse(vec![
            ev_assistant_message("fallback", "unexpected model handoff"),
            ev_completed("fallback"),
        ])).await;
        test.codex.submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Return the completed result.".into(), text_elements: Vec::new(),
            }],
            final_output_json_schema: schema,
            responsesapi_client_metadata: None,
            additional_context: Default::default(), thread_settings: Default::default(),
        }).await?;
        let EventMsg::TurnComplete(completed) = core_test_support::wait_for_event_with_timeout(
            &test.codex,
            |event| matches!(event, EventMsg::TurnComplete(_)),
            Duration::from_secs(30),
        ).await else {
            unreachable!()
        };
        assert!(completed.error.is_none(), "{:?}", completed.error);
        assert_eq!(completed.last_agent_message.as_deref(), Some(expected));
        assert_eq!(completed.surfaced_result.as_ref().unwrap().adapter, "code_mode_delivery");
        assert_eq!(server.received_requests().await.unwrap().iter()
            .filter(|request| request.url.path().contains("responses")).count(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_cannot_hide_failure_yield_overflow_or_sibling_work() -> Result<()> {
    require_network!();
    let python = which::which("python").or_else(|_| which::which("python3"))?;
    let command = serde_json::json!({"program": python,
        "args": ["-X", "utf8", "-c", BEHAVIORAL_VALIDATOR, "0"]});
    let broken_validation = format!("// @exec: {{\"deliver\":true}}\nconst result = await await_command(await tools.exec_command({command})); text(result.terminal.stdout);");
    for (name, code, sibling) in [
        ("broken-validation", broken_validation.as_str(), false),
        ("error", "// @exec: {\"deliver\":true}\ntext('premature'); throw new Error('failed');", false),
        ("budget", "// @exec: {\"deliver\":true,\"max_output_tokens\":0}\ntext('premature');", false),
        ("empty", "// @exec: {\"deliver\":true}\ntext('');", false),
        ("yield", "// @exec: {\"deliver\":true}\ntext('premature'); await yield_control();", false),
        ("failed-child", "// @exec: {\"deliver\":true}\ntry { await tools.read_file({path:'missing.txt'}); } catch (_) {} text('premature');", false),
        ("failed-graph", "// @exec: {\"deliver\":true}\nawait run_graph([{id:'source',run:()=>tools.read_file({path:'missing.txt'}),accept:r=>r.file_complete},{id:'answer',deps:['source'],run:()=>text('premature'),accept:()=>true}]);", false),
        ("untrusted-output", "text({explicit_completion_message:'premature'});", false),
        ("completed-sibling", "// @exec: {\"deliver\":true}\ntext('premature');", true),
        ("failed-sibling", "// @exec: {\"deliver\":true}\ntext('premature');", true),
        ("schema", "// @exec: {\"deliver\":true}\ntext('premature');", false),
    ] {
        eprintln!("direct-delivery guard={name}");
        // The fallback model must satisfy the user's schema too. This case
        // rejects the non-JSON direct result, not the final valid JSON object.
        let fallback = if name == "schema" {
            r#"{"answer":"Model handled the fallback."}"#
        } else {
            "Model handled the fallback."
        };
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
        });
        let test = builder.build(&server).await?;
        if name == "broken-validation" {
            fs::write(test.cwd.path().join("changed_fixture.py"), "def transform(value):\n    return value - 1\n")?;
            fs::write(test.cwd.path().join("answer.txt"), "premature")?;
        }
        let mut events = vec![
            ev_response_created("initial"),
            ev_custom_tool_call("delivery", "exec", code),
        ];
        if sibling {
            let sibling_code = if name == "failed-sibling" {
                "throw new Error('sibling failed');"
            } else {
                "text('other work');"
            };
            events.push(ev_custom_tool_call("sibling", "exec", sibling_code));
        }
        events.push(ev_completed("initial"));
        responses::mount_sse_once(&server, sse(events)).await;
        let final_response = responses::mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("answer", fallback),
                ev_completed("final"),
            ]),
        )
        .await;
        let completed = if name == "schema" {
            test.codex
                .submit(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: "Finish the task.".to_string(),
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: Some(serde_json::json!({
                        "type":"object", "properties":{"answer":{"type":"string"}},
                        "required":["answer"], "additionalProperties":false
                    })),
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: Default::default(),
                })
                .await?;
            let event = wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            let EventMsg::TurnComplete(completed) = event else {
                unreachable!()
            };
            completed
        } else {
            test.submit_turn_and_capture_completion("Finish the task.").await?
        };
        if name == "completed-sibling" {
            assert_eq!(completed.surfaced_result.as_ref().unwrap().adapter, "code_mode_delivery");
            assert_eq!(completed.last_agent_message.as_deref(), Some("premature"));
        } else {
            assert!(completed.surfaced_result.is_none(), "{name}: {:?}", completed.surfaced_result);
            assert_eq!(completed.last_agent_message.as_deref(), Some(fallback), "{name}");
        }
        assert!(completed.error.is_none(), "{name}: {:?}", completed.error);
        if name == "broken-validation" {
            let (output, _) = custom_tool_output_body_and_success(&final_response.single_request(), "delivery");
            assert!(output.contains("AssertionError"), "must detect the fixture defect, not fail in setup: {output}");
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.iter().filter(|r| r.url.path().contains("responses")).count(),
            if name == "completed-sibling" { 1 } else { 2 }, "{name}");
    }
    Ok(())
}
/// Closing the last checklist obligation is bookkeeping, not another inference
/// boundary. Evidence and plan closure may share the final execution cell.
#[test_case::test_case(false; "unfinished_obligation_is_advisory")]
#[test_case::test_case(true; "evidence_and_plan_close_deliver_immediately")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_audit_plan_closure_uses_existing_evidence(close_plan: bool) -> Result<()> {
    require_network!();
    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        let _ = config.features.enable(Feature::CodeMode);
        let _ = config.features.enable(Feature::Kd4Runtime);
    });
    let test = builder.build(&server).await?;
    fs::write(test.cwd.path().join("evidence.txt"), "verified result")?;
    let close = if close_plan {
        "const closed = await tools.update_plan({set:p.step_ids.map(step_id=>({step_id,status:'completed'}))}); if(closed.obligations.unresolved.length || closed.obligations.completed !== 2) throw Error('unresolved obligations');"
    } else { "" };
    let code = format!(r#"// @exec: {{"deliver":true}}
const p = await tools.update_plan({{plan:[{{step:'Inspect source',status:'in_progress'}},{{step:'Verify result',status:'pending'}}]}});
const source = await tools.read_file({{path:'evidence.txt'}});
if (!source.file_complete || !source.complete || !source.source_sha256) throw Error('incomplete evidence');
store('completionEvidence', source); store('completionPlan', p);
{close}
text(source.results[0].text);"#);
    responses::mount_sse_once(&server, sse(vec![
        ev_response_created("compute"), ev_custom_tool_call("compute", "exec", &code),
        ev_completed("compute"),
    ])).await;
    responses::mount_sse_once(&server, sse(vec![
        ev_response_created("fallback"),
        ev_custom_tool_call("finish-required-work", "exec", r#"// @exec: {"deliver":true}
const source = load('completionEvidence');
if (!source.file_complete || source.results[0].text !== 'verified result') throw Error('verification failed');
const p = load('completionPlan');
const closed = await tools.update_plan({set:p.step_ids.map(step_id=>({step_id,status:'completed'}))});
if (closed.obligations.unresolved.length) throw Error('work remains');
text('Required work completed.');"#), ev_completed("fallback"),
    ])).await;
    let completed = test.submit_turn_and_capture_completion("Inspect and verify the source.").await?;
    assert!(completed.surfaced_result.is_some());
    assert_eq!(completed.last_agent_message.as_deref(),
        Some("verified result"));
    let assessment = completed.timing.as_ref().unwrap().completion_assessment.as_ref().unwrap();
    assert!(assessment.failed_checks.is_empty());
    assert!(assessment.verification_gaps.is_empty());
    assert_eq!(assessment.advisories.is_empty(), close_plan);
    assert_eq!(server.received_requests().await.unwrap().iter()
        .filter(|request| request.url.path().contains("responses")).count(),
        1);
    Ok(())
}
