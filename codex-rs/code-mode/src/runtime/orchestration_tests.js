// Shared by the owning V8 runtime and the standalone no-model probe.
const check = (value, why) => { if (!value) throw Error(why); };
const rejected = async action => {
  try { await action(); } catch (error) { return error; }
  throw Error("expected failure");
};
const savedTools = globalThis.tools, savedNames = globalThis.ALL_TOOL_NAMES;
globalThis.ALL_TOOL_NAMES = ["read_file", "read_tool_output", "write_stdin"];
const raw = (text, start = 0) => ({ status: "ok", complete: true,
  canonical_range: { start, end: start + [...text].reduce((n, ch) =>
    n + (ch.codePointAt(0) < 128 ? 1 : ch.codePointAt(0) < 2048 ? 2 : ch.codePointAt(0) < 65536 ? 3 : 4), 0) }, text });
const inline = text => ({ complete: true, file_complete: true, results: [raw(text)] });
const live = (output = "progress", id = 7) => ({ execution_state: "running", session_id: id,
  session_capabilities: { polling: true }, process_exited: false, output });
const done = { execution_state: "exited", process_exited: true, exit_code: 0, output: "" };
try {
  // Bounded parallel reads, exact-path deduplication, stable request order, no
  // discarded successful siblings, and all effects settled on partial failure.
  let calls = [], active = 0, peak = 0, finished = 0;
  globalThis.tools = { read_file: async ({ path }) => {
    calls.push(path); peak = Math.max(peak, ++active);
    await new Promise(resolve => setTimeout(resolve, 1));
    --active; ++finished;
    if (path === "bad") throw Error("read failed");
    return inline(path);
  }};
  const rows = await read_files(["a", "bad", "b", "a", "c"], { concurrency: 2 });
  check(calls.join() === "a,bad,b,c" && peak === 2 && finished === 4 && !active, "batch lifecycle");
  check(rows.map(r => r.path).join() === "a,bad,b,a,c", "result order");
  check(rows[0].value === rows[3].value && rows[1].status === "rejected" &&
    rows[4].value.initial.results[0].text === "c", "deduplication/failure evidence");
  calls = [];
  for (const [paths, options] of [[[], {}], [["a"], { concurrency: 0 }], [["a", null], {}],
    [["a"], { full: "yes" }], [Array(33).fill("a"), {}]]) {
    check(await rejected(() => read_files(paths, options)) instanceof TypeError, "bad batch accepted");
  }
  check(!calls.length, "preflight had partial effects");

  // Inline success must not bypass full-read bounds, nor discard the receipt
  // when its size is unknown or over the per-file share of the aggregate scope.
  for (const size of [undefined, -1, 8 * 1024 * 1024 + 1, 300_000]) {
    const receipt = { ...inline("small fixture"), canonical_bytes: size };
    let recoveryCalls = 0;
    tools.read_file = async () => receipt;
    tools.read_tool_output = async () => { recoveryCalls++; throw Error("unexpected recovery"); };
    const paths = size === 300_000 ? Array.from({length:32}, (_, i) => String(i)) : ["file"];
    const bounded = await read_files(paths, {full:true});
    check(bounded.every(row => row.status === "rejected" && row.reason.evidence.initial === receipt) &&
      recoveryCalls === 0, "inline evidence bypassed full-read scope");
  }
  tools.read_file = async () => ({...inline("ok"), canonical_bytes:2});
  check((await read_files(["file"], {full:true}))[0].value.file_complete, "bounded inline file rejected");

  // Recover exactly the original UTF-8/CRLF bytes. A mutable source is read
  // once, not once per page; the artifact hash and offset must agree each time.
  const initial = { complete: true, file_complete: false, source_sha256: "hash", canonical_bytes: 12,
    artifact_id: "snapshot", retained_artifact_complete: true, results: [raw("a\r\n")],
    continuation: { kind: "bytes", start: 3, end: 12 } };
  const middle = { artifact_id: "snapshot", canonical_sha256: "hash", canonical_bytes: 12,
    complete: false, results: [{ status: "selector_too_large", complete: false,
      child_selectors: [{ kind: "bytes", start: 3, end: 9 }] }, raw("λ😀", 3)],
    continuation_stop: { reason: "budget", resumable: true, selector: { kind: "bytes", start: 9, end: 12 } } };
  const last = { artifact_id: "snapshot", canonical_sha256: "hash", canonical_bytes: 12,
    complete: true, results: [raw("z\r\n", 9)] };
  let reads = 0, offsets = [];
  globalThis.tools = {
    read_file: async () => { reads++; return initial; },
    read_tool_output: async args => {
      check(args.artifact_id === "snapshot" && args.max_bytes === 1048576, "recovery scope");
      offsets.push(args.selectors[0].start);
      return offsets.length === 1 ? middle : last;
    },
  };
  const recovered = (await read_files(["file"], { full: true }))[0];
  check(reads === 1 && offsets.join() === "3,9" && recovered.value.file_complete, "recovery rounds");
  const joined = [recovered.value.initial, ...recovered.value.pages].flatMap(p => p.results).map(p => p.text).join("");
  check(joined === "a\r\nλ😀z\r\n", "source coverage/encoding changed");
  // Every mechanical stop preserves the initial observation and the bad page.
  for (const page of [
    { ...middle, canonical_sha256: "changed" },
    { ...middle, results: [raw("λ", 4)] },
    { ...middle, results: [raw("", 3)] },
    { ...middle, continuation_stop: { reason: "unavailable", resumable: false } },
    { ...middle, results: [] },
    { ...middle, results: [raw("wrong", 3)] },
    { ...last, results: [{...raw("λ😀z\r\n", 3), complete:false}] },
  ]) {
    let polls = 0;
    tools.read_tool_output = async () => { polls++; return page; };
    const row = (await read_files(["file"], { full: true }))[0];
    check(row.status === "rejected" && row.reason.evidence.initial === initial && polls === 1,
      "invalid evidence was accepted or retried");
  }
  tools.read_tool_output = async () => { throw Error("cancelled"); };
  const failed = (await read_files(["file"], { full: true }))[0];
  check(failed.reason.evidence.initial === initial && failed.reason.evidence.cause.message === "cancelled",
    "transport failure lost evidence");

  // A validation process can emit several packets and then an empty exit.
  // Keep every receipt, resume the same process, never start another command.
  let polls = 0;
  tools.write_stdin = async args => {
    check(args.session_id === 7 && args.wait_for_output === true, "non-passive/restarted wait");
    return ++polls < 3 ? live(`packet-${polls}`) : done;
  };
  const terminal = await await_command(live("started"));
  check(polls === 3 && terminal.terminal === done && terminal.observations.length === 4,
    "command did not drain");
  check(terminal.observations.map(r => r.output).join("|") === "started|packet-1|packet-2|",
    "earlier diagnostics lost");
  // The caller may compute a final answer only after checking its full task
  // postcondition. Helpers do not print progress or infer semantic success.
  check((await await_command(done)).observations.length === 1 && polls === 3, "terminal was polled");
  for (const value of [{ ...done, exit_code: 9 }, { ...done, session_id: 7 },
    { ...live(), error: "cancelled" }, { ...live(), pending_deferred_completions: [1] },
    { ...live(), pending_deferred_completions: 1 }, { ...live(), session_capabilities: {} },
    { ...live(), execution_state: "cancelled" }]) {
    const error = await rejected(() => await_command(value));
    check(error.evidence.observations[0] === value && polls === 3, "unsafe automatic continuation");
  }
  for (const value of [
    {...done, execution_state:"running"}, {...done, execution_state:"unknown"},
    {...done, process_exited:undefined}, {...done, error:""},
    {...done, pending_deferred_completions:{}}, {...done, pending_deferred_completions:0},
  ]) {
    const error = await rejected(() => await_command(value));
    check(error.evidence.terminal === value && polls === 3, "ambiguous terminal state accepted");
  }
  check((await rejected(() => await_command(live(), { on_progress: () => false }))).evidence,
    "semantic decision bypassed");
  check((await rejected(() => await_command(live(), { max_observations: 1 }))).evidence,
    "observation limit bypassed");
  tools.write_stdin = async () => live("unexpected", 8);
  const changed = await rejected(() => await_command(live()));
  check(changed.evidence.observations.length === 2, "changed handle silently followed");
  tools.write_stdin = async () => { throw Error("transport closed"); };
  const interrupted = await rejected(() => await_command(live()));
  check(interrupted.evidence.terminal.session_id === 7 && interrupted.evidence.cause.message === "transport closed",
    "interruption lost resumable handle");
  const nullFailure = await rejected(() => await_command(live(), { on_progress: () => { throw null; } }));
  check(nullFailure.evidence.terminal.session_id === 7 && nullFailure.evidence.cause === null,
    "non-Error rejection lost resumable handle");
  check(await rejected(() => await_command(live(), { max_observations: 0 })) instanceof TypeError,
    "bad command bound accepted");
} finally {
  globalThis.tools = savedTools;
  globalThis.ALL_TOOL_NAMES = savedNames;
}
text("orchestration scenarios passed");
