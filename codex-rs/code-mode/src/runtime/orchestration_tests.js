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
  session_capabilities: { polling: true, incarnation: "creation-a" }, process_exited: false, output });
const done = { execution_state: "exited", process_exited: true, exit_code: 0, output: "" };
try {
  globalThis.ALL_TOOL_NAMES = ["read_status"];
  let statusCalls = 0;
  globalThis.tools = { read_status: async ({paths}) => {
    statusCalls++;
    return {paths: paths.map(path => ({path, status:"unknown"}))};
  }};
  check((await read_status(["a", "b"])).paths.length === 2 && statusCalls === 1, "status batch");
  for (const paths of [[], [null], [" "], Array(33).fill("a")]) {
    check(await rejected(() => read_status(paths)) instanceof TypeError, "bad status paths accepted");
  }
  check(statusCalls === 1, "status preflight dispatched invalid paths");
  globalThis.ALL_TOOL_NAMES = ["read_file", "read_tool_output", "write_stdin"];
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
  // when its size is unknown or over the aggregate scope.
  for (const size of [undefined, -1, 8 * 1024 * 1024 + 1]) {
    const receipt = { ...inline("small fixture"), canonical_bytes: size };
    let recoveryCalls = 0;
    tools.read_file = async () => receipt;
    tools.read_tool_output = async () => { recoveryCalls++; throw Error("unexpected recovery"); };
    const bounded = await read_files(["file"], {full:true});
    check(bounded.every(row => row.status === "rejected" && row.reason.evidence.initial === receipt) &&
      recoveryCalls === 0, "inline evidence bypassed full-read scope");
  }
  const unevenPaths = Array.from({length:32}, (_, i) => String(i));
  tools.read_file = async ({path}) => ({...inline(path), canonical_bytes:path === "0" ? 300_000 : 1});
  check((await read_files(unevenPaths, {full:true})).every(row => row.status === "fulfilled"),
    "uneven batch rejected despite fitting aggregate scope");
  // Reproduce the reported 307,231-byte batch with actual delivered bytes,
  // not just size metadata. A complete receipt must not trigger another producer.
  const unevenReceipts = unevenPaths.map((_, i) => {
    const size = i === 0 ? 300_000 : i === 31 ? 241 : 233;
    return {...inline("x".repeat(size)), canonical_bytes:size};
  });
  let unevenReads = 0, unevenRecoveries = 0;
  tools.read_file = async ({path}) => { unevenReads++; return unevenReceipts[Number(path)]; };
  tools.read_tool_output = async () => { unevenRecoveries++; throw Error("unexpected recovery"); };
  const uneven = await read_files(unevenPaths, {full:true});
  check(unevenReceipts.reduce((sum, receipt) => sum + receipt.canonical_bytes, 0) === 307_231 &&
    uneven.every((row, i) => row.status === "fulfilled" && row.value.file_complete &&
      row.value.initial === unevenReceipts[i]) && unevenReads === 32 && unevenRecoveries === 0,
    "reported skewed batch lost delivered evidence or repeated a producer");
  tools.read_file = async () => ({...inline("receipt"), canonical_bytes:300_000});
  const oversized = await read_files(unevenPaths, {full:true});
  check(oversized.filter(row => row.status === "fulfilled").length === 27 &&
    oversized.filter(row => row.status === "rejected").every(row => row.reason.evidence.initial.canonical_bytes === 300_000),
    "aggregate bound or partial evidence lost");
  tools.read_file = async () => ({...inline("ok"), canonical_bytes:2});
  check((await read_files(["file"], {full:true}))[0].value.file_complete, "bounded inline file rejected");
  // Inline full reads require neither recovery discovery nor a recovery call.
  globalThis.ALL_TOOL_NAMES = ["read_file"];
  let inlineReads = 0;
  tools.read_file = async () => { inlineReads++; return {...inline("ok"), canonical_bytes:2}; };
  const inlineOnly = await read_files(["file", "file"], {full:true});
  check(inlineReads === 1 && inlineOnly.every(row => row.status === "fulfilled" && row.value.file_complete),
    "inline full read required an unavailable recovery tool or duplicate read");
  globalThis.ALL_TOOL_NAMES = ["read_file", "read_tool_output", "write_stdin"];

  // Recovery bytes count toward the same 8 MiB bound. Concurrent in-flight
  // snapshots reserve their whole scope once; exact duplicate paths share it.
  {
    const size = 5 * 1024 * 1024;
    let reads = 0, recoveries = 0;
    tools.read_file = async ({path}) => {
      reads++;
      return path === "small" ? {...inline("s".repeat(1024)), canonical_bytes:1024} :
        {complete:true, file_complete:false, canonical_bytes:size, source_sha256:"large-hash",
          artifact_id:"large", retained_artifact_complete:true, results:[raw("x")],
          continuation:{kind:"bytes", start:1, end:size}};
    };
    tools.read_tool_output = async ({artifact_id, selectors, max_bytes}) => {
      recoveries++;
      const start = selectors[0].start, end = Math.min(size, start + max_bytes);
      return {artifact_id, canonical_sha256:"large-hash", canonical_bytes:size,
        complete:end === size, results:[raw("x".repeat(end - start), start)],
        ...(end < size ? {continuation_stop:{reason:"budget", resumable:true,
          selector:{kind:"bytes", start:end, end:size}}} : {})};
    };
    const uneven = await read_files(["large", "small", "large"], {full:true});
    check(reads === 2 && recoveries === 5 && uneven[0].value === uneven[2].value &&
      uneven.every(row => row.status === "fulfilled" && row.value.file_complete),
      "5 MiB snapshot and 1 KiB sibling failed aggregate recovery or repeated reads");
  }
  const snapshotSize = 512 * 1024;
  const snapshotTail = raw("x".repeat(snapshotSize - 1), 1);
  for (const count of [16, 17]) {
    let snapshotReads = 0, snapshotRecoveries = 0;
    tools.read_file = async ({path}) => {
      snapshotReads++;
      return {complete:true, file_complete:false, canonical_bytes:snapshotSize,
        source_sha256:"snapshot-hash", artifact_id:path, retained_artifact_complete:true,
        results:[raw("x")], continuation:{kind:"bytes", start:1, end:snapshotSize}};
    };
    tools.read_tool_output = async ({artifact_id, selectors}) => {
      snapshotRecoveries++;
      await new Promise(resolve => setTimeout(resolve, 1));
      check(selectors.length === 1 && selectors[0].start === 1 && selectors[0].end === snapshotSize,
        "aggregate recovery changed its exact scope");
      return {artifact_id, canonical_sha256:"snapshot-hash", canonical_bytes:snapshotSize,
        complete:true, results:[snapshotTail]};
    };
    const paths = Array.from({length:count}, (_, i) => `snapshot-${i}`);
    const snapshots = await read_files([...paths, paths[0]], {full:true, concurrency:16});
    check(snapshotReads === count && snapshotRecoveries === 16 &&
      snapshots.filter(row => row.status === "fulfilled").length === 17 &&
      snapshots[0].value === snapshots[count].value &&
      snapshots.filter(row => row.status === "fulfilled").every(row =>
        row.value.file_complete && row.value.pages.length === 1 &&
        row.value.pages[0].results[0] === snapshotTail),
      "recovery aggregate accounting lost bytes, double-charged duplicates, or repeated a producer");
    if (count === 17) {
      check(snapshots[16].status === "rejected" &&
        snapshots[16].reason.evidence.initial.artifact_id === paths[16] &&
        snapshots[16].reason.evidence.pages.length === 0,
        "genuine recovery overflow was accepted or discarded its initial receipt");
    }
  }

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
  globalThis.ALL_TOOL_NAMES = ["read_file"];
  tools.read_file = async () => initial;
  const unavailableRecovery = (await read_files(["file"], {full:true}))[0];
  check(unavailableRecovery.status === "rejected" && unavailableRecovery.reason.evidence.initial === initial &&
    unavailableRecovery.reason.evidence.cause instanceof TypeError, "missing recovery discarded readable evidence");
  globalThis.ALL_TOOL_NAMES = ["read_file", "read_tool_output", "write_stdin"];
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
    check(row.reason.evidence.recovery === undefined, "unsafe page advertised a resumable cursor");
  }
  tools.read_tool_output = async () => { throw Error("cancelled"); };
  const failed = (await read_files(["file"], { full: true }))[0];
  check(failed.reason.evidence.initial === initial && failed.reason.evidence.cause.message === "cancelled",
    "transport failure lost evidence");
  check(failed.reason.evidence.recovery.arguments.selectors[0].start === 3,
    "transport failure lost the initial verified cursor");
  {
    let recoveryCalls = 0;
    tools.read_tool_output = async () => {
      if (++recoveryCalls === 1) return middle;
      throw Error("transport closed after progress");
    };
    const interrupted = (await read_files(["file"], {full:true}))[0].reason.evidence;
    check(recoveryCalls === 2 && interrupted.pages[0] === middle &&
      interrupted.recovery.tool === "read_tool_output" &&
      interrupted.recovery.arguments.artifact_id === initial.artifact_id &&
      interrupted.recovery.arguments.selectors[0].start === 9 &&
      interrupted.recovery.arguments.selectors[0].end === 12,
      "resume recipe repeats verified bytes or loses snapshot identity");
  }

  // A validation process can emit several packets and then an empty exit.
  // Keep every receipt, resume the same process, never start another command.
  let polls = 0;
  tools.write_stdin = async args => {
    check(args.session_id === 7 && args.incarnation === "creation-a" && args.wait_for_output === false, "non-passive/restarted wait");
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
  const certifiedMiss = {...done, exit_code:1, search_no_match:true};
  check((await await_command(certifiedMiss)).terminal === certifiedMiss, "certified negative evidence rejected");
  for (const value of [{...done, exit_code:1}, {...done, exit_code:1, search_no_match:"true"},
    {...done, exit_code:2, search_no_match:true}]) {
    check((await rejected(() => await_command(value))).evidence.terminal === value, "uncertified failure accepted");
  }
  const tail = {...done, session_id:7, session_capabilities:{polling:true, incarnation:"creation-a"}, output:"tail"};
  let tailPolls = 0;
  tools.write_stdin = async args => {
    check(args.session_id === 7 && args.wait_for_output === false, "tail drain changed handle");
    return ++tailPolls < 3 ? {...tail, output:`tail-${tailPolls}`} : done;
  };
  const drained = await await_command(tail);
  check(tailPolls === 3 && drained.observations.length === 4 &&
    drained.observations.map(r => r.output).join("|") === "tail|tail-1|tail-2|",
    "exited output lost, duplicated, or prematurely completed");
  tools.write_stdin = async () => ({...done, exit_code:2});
  check((await rejected(() => await_command({...tail, exit_code:2}))).evidence.observations.length === 2,
    "failed process tail was not drained");
  tools.write_stdin = async () => ({...tail, session_id:8});
  check((await rejected(() => await_command(live()))).evidence.observations.length === 2,
    "changed exited handle followed");
  for (const code of [9, -1, undefined]) {
    const value = { ...done, exit_code: code };
    const error = await rejected(() => await_command(value));
    check(error.message.includes(`exit_code=${code ?? "unknown"}`) &&
      error.evidence.terminal === value, "command failure omitted exit code or evidence");
  }
  let unexpectedPolls = 0;
  tools.write_stdin = async () => { ++unexpectedPolls; throw Error("unsafe receipt was polled"); };
  for (const value of [{ ...done, exit_code: 9 }, { ...done, session_id: 7 },
    { ...live(), error: "cancelled" }, { ...live(), pending_deferred_completions: [1] },
    { ...live(), pending_deferred_completions: 1 }, { ...live(), session_capabilities: {} },
    { ...live(), execution_state: "cancelled" }]) {
    const error = await rejected(() => await_command(value));
    check(error.evidence.observations[0] === value && unexpectedPolls === 0, "unsafe automatic continuation");
  }
  for (const value of [
    {...done, execution_state:"running"}, {...done, execution_state:"unknown"},
    {...done, process_exited:undefined}, {...done, error:""},
    {...done, pending_deferred_completions:{}}, {...done, pending_deferred_completions:0},
  ]) {
    const error = await rejected(() => await_command(value));
    check(error.evidence.terminal === value && unexpectedPolls === 0, "ambiguous terminal state accepted");
  }
  check((await rejected(() => await_command(live(), { on_progress: () => false }))).evidence,
    "semantic decision bypassed");
  check((await rejected(() => await_command(live(), { max_observations: 1 }))).evidence,
    "observation limit bypassed");
  tools.write_stdin = async () => live("unexpected", 8);
  const changed = await rejected(() => await_command(live()));
  check(changed.evidence.observations.length === 2, "changed handle silently followed");
  tools.write_stdin = async () => ({...live(), session_capabilities:{polling:true, incarnation:"creation-b"}});
  check((await rejected(() => await_command(live()))).evidence.observations.length === 2,
    "same numeric handle from a different creation was followed");
  tools.write_stdin = async () => { throw Error("transport closed"); };
  const interrupted = await rejected(() => await_command(live()));
  check(interrupted.evidence.terminal.session_id === 7 && interrupted.evidence.cause.message === "transport closed",
    "interruption lost resumable handle");
  const nullFailure = await rejected(() => await_command(live(), { on_progress: () => { throw null; } }));
  check(nullFailure.evidence.terminal.session_id === 7 && nullFailure.evidence.cause === null,
    "non-Error rejection lost resumable handle");
  // Byte verification must preserve UTF-8 widths, CRLF, and lone-surrogate
  // replacement widths without allocating an iterator per source character.
  for (const tail of ["ascii", "λ", "\u0800", "😀", "\ud800", "\udc00", "\ud800x", "\ud800\ud800\udc00", "\r\n"]) {
    const part = raw(tail, 1), size = part.canonical_range.end;
    tools.read_file = async () => ({complete:true,file_complete:false,source_sha256:"unicode",
      canonical_bytes:size,artifact_id:"unicode",retained_artifact_complete:true,
      results:[raw("x")],continuation:{kind:"bytes",start:1,end:size}});
    tools.read_tool_output = async () => ({complete:true,canonical_sha256:"unicode",
      canonical_bytes:size,artifact_id:"unicode",results:[part]});
    const [result] = await read_files(["unicode"], {full:true});
    check(result.status === "fulfilled" && result.value.pages[0].results[0] === part,
      "UTF-8 recovery changed byte coverage or evidence");
  }
  // A quiet process, and one that keeps printing, both return control within
  // the budget without losing diagnostics or restarting/terminating the owner.
  const realNow = Date.now;
  try {
    for (const output of ["", "still running"]) {
      let now = 0, boundedPolls = 0;
      Date.now = () => now;
      tools.write_stdin = async args => {
        boundedPolls++;
        check(args.wait_for_output === false && args.yield_time_ms === 300_000 &&
          args.session_id === 7 && args.incarnation === "creation-a" &&
          args.terminate === undefined && args.chars === undefined, "unbounded or mutating poll");
        now += args.yield_time_ms;
        return live(output);
      };
      const bounded = await rejected(() => await_command(live("started")));
      check(bounded.message.includes("wait budget reached") && boundedPolls === 1 &&
        bounded.evidence.observations.length === 2 &&
        bounded.evidence.terminal.output === output &&
        bounded.evidence.terminal.session_id === 7, "wait budget lost resumable evidence");
      tools.write_stdin = async () => done;
      check((await await_command(bounded.evidence.terminal)).terminal === done, "cannot resume bounded wait");
    }
    let now = 0, waits = [];
    Date.now = () => now;
    tools.write_stdin = async args => {
      waits.push(args.yield_time_ms);
      now += Math.min(120_000, args.yield_time_ms);
      return live("progress is not completion");
    };
    const progressing = await rejected(() => await_command(live()));
    check(waits.join() === "300000,180000,60000" &&
      progressing.evidence.observations.length === 4, "progress reset the total wait budget");
    Date.now = () => 0;
    tools.write_stdin = async args => {
      check(args.yield_time_ms === 5_000, "explicit wait budget ignored");
      return done;
    };
    check((await await_command(live(), {max_wait_ms:5_000})).terminal === done, "bounded completion failed");
    for (const value of [0, -1, 4_999, 300_001, NaN, Infinity, "5000"]) {
      check(await rejected(() => await_command(live(), {max_wait_ms:value})) instanceof TypeError,
        "invalid wait budget accepted");
    }
  } finally { Date.now = realNow; }
  // Recovery calls release admission between pages. Neither large file can
  // recover until the queued small read runs; per-file worker leases deadlock.
  {
    let releaseSmall, reads = 0, recoveries = 0, active = 0, peak = 0;
    const smallRead = new Promise(resolve => { releaseSmall = resolve; });
    tools.read_file = async ({path}) => {
      ++reads; peak = Math.max(peak, ++active);
      try {
        if (path === 'small') {
          releaseSmall();
          return {...inline('s'), canonical_bytes:1};
        }
        return {complete:true,file_complete:false,canonical_bytes:2,source_sha256:path,
          artifact_id:path,retained_artifact_complete:true,results:[raw('a')],
          continuation:{kind:'bytes',start:1,end:2}};
      } finally { --active; }
    };
    tools.read_tool_output = async ({artifact_id}) => {
      ++recoveries; peak = Math.max(peak, ++active);
      try {
        await smallRead;
        return {artifact_id,canonical_sha256:artifact_id,canonical_bytes:2,
          complete:true,results:[raw('b',1)]};
      } finally { --active; }
    };
    const rows = await read_files(['large-a','large-b','small','large-a'], {full:true,concurrency:2});
    check(reads === 3 && recoveries === 2 && peak <= 2 && active === 0 &&
      rows.every(row => row.status === 'fulfilled' && row.value.file_complete) &&
      rows[0].value === rows[3].value, 'recovery admission lost fairness, bounds, or deduplication');
  }
  // Do not spend a fresh native five-second minimum on a sub-minimum remainder.
  {
    const savedNow = Date.now;
    try {
      for (const elapsed of [1, 4999, 5000]) {
        let now = 0, polls = 0;
        Date.now = () => now;
        tools.write_stdin = async () => { ++polls; return done; };
        const stopped = await rejected(() => await_command(live(), {
          max_wait_ms:5000, on_progress:() => { now = elapsed; return true; },
        }));
        check(polls === 0 && stopped.message.includes('wait budget reached') &&
          stopped.evidence.terminal.session_id === 7 && stopped.evidence.observations.length === 1,
          'poll floor exceeded total budget or lost the existing handle');
      }
    } finally { Date.now = savedNow; }
  }
  // An owner-observed stall stops draining immediately, without killing the
  // process or losing earlier output. Ordinary quiet packets are not stalls.
  {
    const stalled = {...live(""), session_capabilities:{...live().session_capabilities,
      observation:{reason:"no_output_observed", silent_for_ms:60_000,
        process_exited:false, termination_requested:false}}};
    for (const initialStall of [true, false]) {
      let polls = 0, progressCalls = 0;
      tools.write_stdin = async args => {
        check(args.terminate === undefined && args.chars === undefined, "stall changed process lifetime");
        ++polls;
        return polls === 1 ? stalled : done;
      };
      const stopped = await rejected(() => await_command(initialStall ? stalled : live("completed work"), {
        on_progress:() => { ++progressCalls; return true; },
      }));
      check(stopped.message.includes("reported no output") && polls === (initialStall ? 0 : 1) &&
        progressCalls === (initialStall ? 0 : 1) && stopped.evidence.terminal === stalled &&
        stopped.evidence.observations.length === (initialStall ? 1 : 2),
        "stall notice was ignored or lost its resumable receipt");
      if (!initialStall) check(stopped.evidence.observations[0].output === "completed work", "lost partial work");
      // After an explicit decision the caller polls the retained handle, then
      // passes that fresh observation back. No process creation is involved.
      tools.write_stdin = async args => {
        check(args.session_id === stalled.session_id && args.incarnation === "creation-a", "resume changed owner");
        return done;
      };
      const resumed = await tools.write_stdin({session_id:stalled.session_id, incarnation:"creation-a"});
      check((await await_command(resumed)).terminal === done, "stalled owner could not be resumed");
    }
    let polls = 0;
    tools.write_stdin = async () => { ++polls; return done; };
    check((await await_command(live(""))).terminal === done && polls === 1, "ordinary silence stopped draining");
    const exitedTail = {...stalled, execution_state:"exited", process_exited:true, exit_code:0};
    check((await await_command(exitedTail)).terminal === done && polls === 2, "old stall prevented final output drain");
  }
  check(await rejected(() => await_command(live(), { max_observations: 0 })) instanceof TypeError,
    "bad command bound accepted");
} finally {
  globalThis.tools = savedTools;
  globalThis.ALL_TOOL_NAMES = savedNames;
}
text("orchestration scenarios passed");
