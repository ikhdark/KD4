//! Focused, local-revision A/B front end to Repo Benchmark's native live runner.
//! No scripted provider, upstream checkout, installed app, or performance verdict.
use anyhow::{Context, Result, ensure};
use repo_benchmark::native::{NativeAttemptRequest, run_attempt};
use repo_benchmark::prepare::environment::{BASE_CONFIG, Environment};
use repo_benchmark::prepare::provenance::{FileIdentity, hash_bytes};
use repo_benchmark::runner::TemporaryAuthentication;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    repo: PathBuf,
    app_server: PathBuf,
    auth_home: PathBuf,
    output: PathBuf,
    repetitions: usize,
    cases: Vec<u32>,
    timeout_ms: u64,
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let file = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(file,value)?;
    Ok(())
}

fn fixture(finding: u32) -> Result<(String,String,Value)> {
    let common = "This is a read-only evidence synthesis task. Use read_file/read_tool_output, optionally code mode for orchestration or arithmetic; no shell commands, external services, or file edits. Do not inspect parent directories. Return only the requested JSON object. Preserve contradictory/negative evidence and do not equate an excerpt with whole-file coverage. Avoid redundant reads when complete evidence has already been delivered. ";
    Ok(match finding {
        5 | 11 => {
            // #11 must exceed the recovery transaction ceiling; a selection
            // already completed by the snapshot owner cannot exercise draining.
            let padding = if finding == 11 { 32 } else { 10 };
            let text = (1..=96).map(|i| format!("ROW{i:03} amount={} {}\n", i%7-3, "verified archival context; ".repeat(padding))).collect::<String>();
            let sum: i32 = (1..=96).map(|i| i%7-3).sum();
            let instruction = if finding == 5 {
                "For every recovery exec cell, use the first-line pragma // @exec: {\"max_output_tokens\": 2000}. Make only one read_tool_output call per cell and print its complete returned object with text; do not extract/summarize fields inside JavaScript. Recover the full snapshot with the provided continuations."
            } else {
                "Your first read_tool_output call must batch an intentionally invalid empty-query search selector {kind:'search',query:''} and the full-file lines selection {kind:'lines',start:1,end:96}. Preserve the invalid search as negative evidence, but recover the independent valid selection. Do not repeat the invalid selector."
            };
            let mut expected = json!({"count":96,"sum":sum,"first":"ROW001","last":"ROW096"});
            if finding == 11 { expected["invalid_selector_reported"] = json!(true); }
            (text,format!("{common}First read bytes 0 through 32 of ledger.txt to obtain a retained snapshot ID. {instruction} Read all rows, including signed amounts. Return JSON with exactly these keys: count (row count), sum (signed total), first (first row ID), last (last row ID){}.",if finding==11 {", and invalid_selector_reported (true only if the invalid selector was reported)"} else {""}),expected)
        },
        14 => (
            "Benchmark uses ledger-a.txt through ledger-d.txt; this file is not evidence.\n".into(),
            format!("{common}First execute exactly this script, without printing or storing the results: for (const suffix of ['a','b','c','d']) {{ await tools.read_file({{path:'ledger-'+suffix+'.txt',selectors:[{{kind:'lines',start:1,end:9}}]}}); }} throw new Error('intentional late failure after four successful reads'); Then recover the successful evidence without replaying the failed cell. Use existing recovery handles where available; read a source again only if its result is inaccessible. Return {{\"tails\":[tail amounts for a,b,c,d],\"sum\":their signed sum}}."),
            json!({"tails":[11,17,-5,23],"sum":46}),
        ),
        21 | 22 => {
            let step = if finding == 21 {
                "First read ledger.txt with environment_id='local' and explicit lines 1 through 5, in its own exec cell."
            } else {
                "First run exactly git status --short --untracked-files=all -- ledger.txt using exec_command, in its own exec cell. Then read all five lines of ledger.txt in a separate exec cell."
            };
            let mut expected = json!({"sum":40,"entries":3,"tail":"violet"});
            if finding == 22 {expected["status"] = json!(["?? ledger.txt"]);}
            ("ALPHA=17\r\nBETA=29\r\nGAMMA=-6\r\nRULE: include signed values\r\nTAIL_CHECK=violet\r\n".into(),
             format!("This is a local evidence-freshness benchmark. Do not inspect parent directories or use external services. Do not modify ledger.txt or Git metadata; no staging or commits. {step} Next, in a separate exec cell, use apply_patch to change notes.txt from phase=before to phase=after, preserving its newline. This is the only authorized file edit. After that edit, synthesize the ledger sum, entries count and tail from current evidence, revalidating evidence only if it is invalidated; do not rerun unchanged valid reads. Return only JSON with sum, entries, tail{}.",if finding==22 {", and status (an array of exact nonempty lines from the scoped Git status output, excluding command wrapper metadata)"} else {""}),expected)
        },
        13 => (
            "ALPHA=17\r\nBETA=29\r\nGAMMA=-6\r\nRULE: include signed values\r\nTAIL_CHECK=violet\r\n".into(),
            format!("{common}Start by reading ledger.txt with explicit line selector 1 through 5. Establish whole-file coverage and report {{\"sum\":integer,\"entries\":integer,\"tail\":string}} from the complete file."),
            json!({"sum":40,"entries":3,"tail":"violet"}),
        ),
        12 => {
            let text = (1..=8).map(|i|format!("ALPHA BETA id={i} value={} {}\n",11*i+3,"archival context not an additional record; ".repeat(20))).collect::<String>();
            (text,format!("{common}First read bytes 0 through 32 of ledger.txt to obtain its retained snapshot ID. Then batch two search selectors for ALPHA and BETA with max_results=100 and context_lines=0 in one read_tool_output call. Use their evidence to report {{\"ids\":[all unique integer ids in order],\"sum\":sum of unique record values}}. An overlapping result is not another record."),json!({"ids":[1,2,3,4,5,6,7,8],"sum":420}))
        },
        10 => {
            let mut errors = Vec::new(); let mut warnings = Vec::new();
            let text = (1..=40).map(|i| {
                let label = if i%3==0 {errors.push(i);"ERROR"} else if i%5==0 {warnings.push(i);"WARN"} else {"NOTICE"};
                format!("{label} record={i} {}\n","routine context with no further classification labels; ".repeat(10))
            }).collect::<String>();
            let expected = json!({"error_count":errors.len(),"warn_count":warnings.len(),"error_lines":errors,"warn_lines":warnings});
            (text,format!("{common}First read bytes 0 through 32 of ledger.txt to obtain its retained snapshot ID. Search the snapshot for ERROR and WARN (max_results=100). You need only counts and one-based line numbers, not excerpt text; choose the available selector most appropriate to that requirement. Return {{\"error_count\":integer,\"warn_count\":integer,\"error_lines\":[integers],\"warn_lines\":[integers]}}."),expected)
        },
        18 => {
            let text = (1..=72).map(|i|format!("id={i:03} amount={} evidence=FACT_{i:03} {}\n",(i-1)%9+1,"archival context preserves this verified record; ".repeat(10))).collect::<String>();
            (text,format!("{common}Read all of ledger.txt using an explicit lines 1 through 72 selection, recovering any incomplete selection. After consuming the evidence, call context_checkpoint as a standalone tool (not inside code mode), selecting the completed read call IDs and retaining all constraints and factual results needed to finish. To exercise a large working-note checkpoint, put at least 2200 but at most 5000 UTF-8 bytes of substantive verified record notes in summary; no padding or invented facts. Keep active_work concise. Then synthesize {{\"count\":integer,\"sum\":integer,\"first_id\":string,\"last_id\":string,\"last_amount\":integer}} without rereading checkpointed evidence unless necessary."),json!({"count":72,"sum":360,"first_id":"001","last_id":"072","last_amount":9}))
        },
        _ => anyhow::bail!("unknown finding {finding}"),
    })
}

fn final_json(events: &[Value]) -> Option<Value> {
    events.iter().rev().find_map(|event| {
        let message = &event["message"];
        if message["method"] != "item/completed" {return None;}
        let item = &message["params"]["item"];
        if item["type"] != "agentMessage" {return None;}
        let text = item["text"].as_str()?.trim();
        let text = text.strip_prefix("```json").or_else(||text.strip_prefix("```"))
            .and_then(|s|s.strip_suffix("```")).unwrap_or(text).trim();
        serde_json::from_str(text).ok()
    })
}

fn main() -> Result<()> {
    let input_path = std::env::args_os().nth(1).context("usage: generation-opportunities INPUT.json")?;
    let input: Input = serde_json::from_slice(&fs::read(input_path)?)?;
    ensure!((1..=3).contains(&input.repetitions),"repetitions must be 1..=3");
    ensure!(!input.cases.is_empty() && input.cases.iter().all(|id|[5,11,14,21,22,13,12,10,18].contains(id)),"invalid cases");
    ensure!((10_000..=300_000).contains(&input.timeout_ms),"timeout must be 10..300 seconds");
    ensure!(input.auth_home.join("auth.json").is_file(),"no authentication file in supplied fork home");
    fs::create_dir(&input.output).context("output must be a new directory")?;
    let output = input.output.canonicalize()?;
    let binaries = output.join("bin"); fs::create_dir(&binaries)?;
    let executable_name = input.app_server.file_name().context("app-server filename")?;
    let executable = binaries.join(executable_name);
    fs::copy(&input.app_server,&executable)?;
    let executable_identity = FileIdentity::record(&executable)?;
    let helper_source = input.app_server.parent().context("app-server parent")?.join("codex-code-mode-host.exe");
    let helper = binaries.join("codex-code-mode-host.exe"); fs::copy(&helper_source,&helper)?;
    let helper_identity = FileIdentity::record(&helper)?;
    let environment = Environment::capture(&input.repo)?;
    let config = format!("{BASE_CONFIG}\n[features]\nkd4_runtime = true\ncode_mode = true\ncode_mode_host = true\n");
    let expected_config = serde_json::to_value(toml::from_str::<toml::Value>(&config)?)?;
    let analyzer_dir = output.join("analyzer"); fs::create_dir(&analyzer_dir)?;
    let mut analyzer_files = Vec::new();
    for name in ["kd4_turn_latency_audit.py","kd4_timing_analysis.py","kd4_first_useful_action_analysis.py","rollout_snapshot.py","atomic_json.py","kd4_session_diagnostics.py"] {
        let path = analyzer_dir.join(name); fs::copy(input.repo.join("scripts").join(name),&path)?;
        analyzer_files.push(FileIdentity::record(&path)?);
    }
    let source_paths = ["core/Cargo.toml","core/src/lib.rs","core/src/generation_live_bench.rs","core/src/tool_history.rs","core/src/tools/code_mode/mod.rs","core/src/tools/code_mode/execute_handler.rs","core/src/tools/code_mode/wait_handler.rs","core/src/tools/command_output_artifact.rs","core/src/tools/handlers/read_file.rs","core/src/tools/handlers/read_tool_output.rs","core/src/tools/handlers/read_tool_output_spec.rs","repo-benchmark/src/bin/generation-opportunities.rs"];
    let source_files = source_paths.iter().map(|p|FileIdentity::record(&input.repo.join("codex-rs").join(p))).collect::<Result<Vec<_>>>()?;
    write_json(&output.join("manifest.json"),&json!({"kind":"integrated_live_model","app_server":executable_identity,"helper":helper_identity,"source_files":source_files,"environment":environment,"config":expected_config,"cases":input.cases,"repetitions":input.repetitions,"timeout_ms":input.timeout_ms,"order":"case order; alternating arm order by repetition","limits":"synthetic instrumented tasks; not a general performance or semantic-quality guarantee"}))?;
    let mut summaries = Vec::new();
    for finding in &input.cases {
        let (source,prompt,expected) = fixture(*finding)?;
        // #18 projects standalone checkpoint arguments, not JavaScript source.
        // Request direct tools. Model metadata can override these flags, so a
        // completed turn is not proof of exercise: inspect the hook and trace.
        let config = if *finding == 18 {
            format!("{BASE_CONFIG}\n[features]\nkd4_runtime = true\ncode_mode = false\ncode_mode_only = false\n")
        } else { config.clone() };
        let expected_config = serde_json::to_value(toml::from_str::<toml::Value>(&config)?)?;
        let prompt = if *finding == 18 {
            format!("{prompt} completed_call_ids must be the completed top-level function call IDs, not artifact IDs or numeric cell IDs. Keep retained_evidence empty; it must not overlap completed_call_ids.")
        } else { prompt };
        for repetition in 0..input.repetitions {
            let arms = if repetition%2==0 {[false,true]} else {[true,false]};
            for candidate in arms {
                executable_identity.verify()?; helper_identity.verify()?;
                let arm = if candidate {"candidate"} else {"baseline"};
                let id = format!("finding-{finding}-pair-{repetition}-{arm}");
                let evidence_dir = output.join(&id); fs::create_dir(&evidence_dir)?;
                // Keep fixture/homes outside the checkout so project config and
                // repository instructions cannot leak into one benchmark arm.
                let root = tempfile::Builder::new().prefix("kd4-generation-live-").tempdir()?.keep();
                let workspace = root.join("workspace"); let home = root.join("home");
                fs::create_dir(&workspace)?; fs::create_dir(&home)?;
                fs::write(workspace.join("ledger.txt"),&source)?;
                let mut fixtures = std::collections::BTreeMap::from([("ledger.txt".to_string(),source.clone())]);
                if *finding == 14 {
                    for (suffix, amount) in [('a',11),('b',17),('c',-5),('d',23)] {
                        let name = format!("ledger-{suffix}.txt");
                        let mut text = (1..=8).map(|i|format!("context {i}: verified archival statement, not an amount; {}\n","retain this evidence; ".repeat(8))).collect::<String>();
                        text.push_str(&format!("TAIL={amount}\n"));
                        fs::write(workspace.join(&name),&text)?;
                        fixtures.insert(name,text);
                    }
                }
                if [21,22].contains(finding) {
                    fs::write(workspace.join("notes.txt"),"phase=before\n")?;
                    fixtures.insert("notes.txt".into(),"phase=before\n".into());
                    let mut git = std::process::Command::new(&environment.tools["git"].executable.path);
                    git.args(["init","--quiet"]).current_dir(&workspace);
                    #[cfg(windows)] { use std::os::windows::process::CommandExt; git.creation_flags(0x08000000); }
                    ensure!(git.status()?.success(),"initialize isolated Git fixture");
                }
                fs::write(home.join("config.toml"),&config)?;
                let fixture_hash = hash_bytes(source.as_bytes());
                write_json(&evidence_dir.join("inputs.json"),&json!({"finding":finding,"candidate":candidate,"repetition":repetition,"prompt":prompt,"source":source,"fixtures":fixtures,"expected":expected,"config":expected_config,"fixture_sha256":fixture_hash,"workspace":workspace,"home":home}))?;
                let mut env = environment.variables.clone();
                env.insert("CODEX_CODE_MODE_HOST_PATH".into(),helper.to_string_lossy().into_owned());
                env.insert("CODEX_ROLLOUT_TRACE_ROOT".into(),evidence_dir.join("trace").to_string_lossy().into_owned());
                env.insert("KD4_GENERATION_BENCH_LOG".into(),evidence_dir.join("candidate-exercise.jsonl").to_string_lossy().into_owned());
                if candidate {env.insert("KD4_GENERATION_BENCH_CANDIDATE".into(),finding.to_string());}
                else {env.insert("KD4_GENERATION_BENCH_BASELINE".into(),finding.to_string());}
                let mut auth = TemporaryAuthentication::install(Some(&input.auth_home.join("auth.json")),&home.join("auth.json"))?;
                let native = run_attempt(&NativeAttemptRequest {attempt_id:id.clone(),app_server:executable.clone(),cwd:workspace.clone(),codex_home:home,evidence_dir:evidence_dir.clone(),env,config_overrides:vec![],expected_config:expected_config.clone(),prompt:prompt.clone(),timeout_ms:input.timeout_ms,scenario:None});
                auth.remove()?;
                let answer = final_json(&native.events);
                let unchanged = fixtures.iter().filter(|(name,_)|name.as_str()!="notes.txt")
                    .all(|(name,text)|fs::read(workspace.join(name)).is_ok_and(|bytes|bytes==text.as_bytes()));
                let edit_correct = ![21,22].contains(finding) || fs::read(workspace.join("notes.txt")).is_ok_and(|bytes|bytes==b"phase=after\n");
                let correct = answer.as_ref()==Some(&expected) && unchanged && edit_correct && native.status=="completed";
                let diagnostics = repo_benchmark::diagnostics::analyze(&environment.tools["python"].executable.path,&analyzer_dir.join("kd4_turn_latency_audit.py"),&analyzer_files,&native,&evidence_dir.join("diagnostics"),&workspace,true,None);
                let exercised = evidence_dir.join("candidate-exercise.jsonl").is_file();
                let summary = json!({"finding":finding,"arm":arm,"repetition":repetition,"native_status":native.status,"correct":correct,"answer":answer,"unchanged_source":unchanged,"edit_correct":edit_correct,"candidate_hook_observed":exercised,"turn_elapsed_ms":native.turn_elapsed_ms,"elapsed_ms":native.elapsed_ms,"cleanup_ms":native.cleanup_ms,"tool_executions":native.tool_executions,"diagnostics":diagnostics,"evidence":native.evidence_path});
                write_json(&evidence_dir.join("summary.json"),&summary)?;
                println!("{id}: status={}, correct={correct}, hook={exercised}, turn_ms={:?}",native.status,native.turn_elapsed_ms);
                summaries.push(summary);
                // Fail closed on transport/auth/setup failure; don't spend live
                // requests retrying an unchanged broken environment.
                if native.status != "completed" {
                    write_json(&output.join("results.json"),&summaries)?;
                    anyhow::bail!("native attempt failed; evidence retained at {}",evidence_dir.display());
                }
            }
        }
    }
    write_json(&output.join("results.json"),&summaries)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_and_final_answer_contracts() {
        for id in [5,11,14,21,22,13,12,10,18] {
            let (source,prompt,expected) = fixture(id).unwrap();
            assert!(!source.is_empty() && prompt.contains("read"));
            let event = json!({"message":{"method":"item/completed","params":{"item":{"type":"agentMessage","text":expected.to_string()}}}});
            assert_eq!(final_json(&[event]),Some(expected));
            assert_eq!(final_json(&[json!({"message":{"method":"item/completed","params":{"item":{"type":"commandExecution","text":"{}"}}}})]),None);
        }
        assert_eq!(fixture(10).unwrap().2["error_count"],13);
        assert_eq!(fixture(10).unwrap().2["warn_count"],6);
        assert_eq!(fixture(18).unwrap().0.lines().count(),72);
    }
}
