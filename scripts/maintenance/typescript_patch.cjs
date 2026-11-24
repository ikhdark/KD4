// Preview bounded TypeScript language-service edits as one guarded native patch.
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

function preview(spec, ts) {
  const { op, file, symbol, value, files } = spec;
  if (!Array.isArray(files) || !files.length || files.length > 64 ||
      !["rename", "move", "signature"].includes(op))
    throw new Error("expected a supported operation and 1–64 explicit project files");
  const root = fs.realpathSync(process.cwd()).replaceAll("\\", "/");
  const clean = name => {
    if (typeof name !== "string" || !name.endsWith(".ts") || path.isAbsolute(name) ||
        name.includes("\\") || name.includes(":") || /[\r\n\0]/.test(name) ||
        name.split("/").some(p => !p || p.trim() !== p || p === "." || p === ".."))
      throw new Error("expected a normalized project-relative .ts path");
    return root + "/" + name;
  };
  const hash = value => crypto.createHash("sha256").update(value).digest("hex");
  const sources = new Map();
  let total = 0;
  for (const [name, expected] of files) {
    const full = clean(name);
    if (sources.has(full) || fs.realpathSync(full).replaceAll("\\", "/") !== full)
      throw new Error("duplicate or redirected source: " + name);
    const fd = fs.openSync(full, "r");
    let raw;
    try {
      const buffer = Buffer.alloc(1024 * 1024 - total + 1);
      let count = 0;
      while (count < buffer.length) {
        const n = fs.readSync(fd, buffer, count, buffer.length - count, null);
        if (!n) break;
        count += n;
      }
      raw = buffer.subarray(0, count);
    } finally {
      fs.closeSync(fd);
    }
    total += raw.length;
    if (total > 1024 * 1024 || hash(raw) !== expected)
      throw new Error("source budget exceeded or source hash changed: " + name);
    const text = raw.toString("utf8");
    if (!raw.equals(Buffer.from(text))) throw new Error("invalid UTF-8 source: " + name);
    sources.set(full, { name, text, expected });
  }
  const selected = clean(file);
  if (!sources.has(selected)) throw new Error("target is not in the declared project");
  const directories = new Set([root]);
  for (const name of sources.keys())
    for (let dir = path.posix.dirname(name); dir !== root; dir = path.posix.dirname(dir))
      directories.add(dir);
  const options = { target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.CommonJS, types: [] };
  const lib = ts.getDefaultLibFilePath(options).replaceAll("\\", "/");
  const libDir = path.posix.dirname(lib);
  const normalize = name => name.replaceAll("\\", "/");
  const isLib = name => path.posix.dirname(name) === libDir && /^lib\.[\w.-]+\.d\.ts$/.test(path.posix.basename(name));
  const read = n => {
    const name = normalize(n);
    return sources.get(name)?.text ?? (isLib(name) ? ts.sys.readFile(name) : undefined);
  };
  const svc = ts.createLanguageService({
    getCompilationSettings: () => options, getScriptFileNames: () => [...sources.keys()],
    getScriptVersion: () => "1", getCurrentDirectory: () => root,
    getDefaultLibFileName: () => lib, fileExists: name => read(name) !== undefined, readFile: read,
    directoryExists: name => directories.has(normalize(name)) || normalize(name) === libDir,
    getScriptSnapshot: name => {
      const text = read(name);
      return text === undefined ? undefined : ts.ScriptSnapshot.fromString(text);
    },
  });
  const project = svc.getProgram();
  for (const name of sources.keys()) {
    if (svc.getSyntacticDiagnostics(name).length ||
        svc.getSemanticDiagnostics(name).some(d => d.code === 2307 || d.code === 2792))
      throw new Error("invalid syntax or unresolved module in declared project: " + name);
  }
  const format = { insertSpaceAfterCommaDelimiter: true };
  let edits, destination;
  if (op === "move") {
    destination = clean(value);
    const parent = path.posix.dirname(destination);
    if (fs.realpathSync(parent).replaceAll("\\", "/") !== parent || fs.existsSync(destination))
      throw new Error("move destination must be absent under an existing nonredirected parent");
    edits = svc.getEditsForFileRename(selected, destination, format, {});
  } else {
    const source = project.getSourceFile(selected);
    const matches = source.statements.filter(n => ts.isFunctionDeclaration(n) && n.name?.text === symbol && n.body);
    if (matches.length !== 1) throw new Error("expected one named top-level function definition");
    const pos = matches[0].name.getStart(source);
    if (op === "rename") {
      if (typeof value !== "string" || !/^[A-Za-z_$][\w$]*$/.test(value) || value === symbol)
        throw new Error("expected a distinct identifier");
      const hasIdentifier = node => (ts.isIdentifier(node) && node.text === value) ||
        Boolean(ts.forEachChild(node, child => hasIdentifier(child) || undefined));
      if ([...sources.keys()].some(name => hasIdentifier(project.getSourceFile(name))))
        throw new Error("new identifier is already present in the declared project");
      edits = svc.findRenameLocations(selected, pos, false, false, true)?.map(e => ({
        fileName: e.fileName,
        textChanges: [{ span: e.textSpan, newText: (e.prefixText ?? "") + value + (e.suffixText ?? "") }],
      }));
    } else {
      const name = "Convert parameters to destructured object";
      const group = svc.getApplicableRefactors(selected, pos, {}, undefined, undefined, true)
        .find(group => group.name === name);
      const action = group?.actions.find(a => !a.notApplicableReason);
      edits = action && svc.getEditsForRefactor(selected, format, pos, name, action.name, {})?.edits;
    }
  }
  if (!edits?.length && !destination) throw new Error("language service did not return edits");
  const groups = new Map();
  for (const edit of edits ?? []) {
    const name = normalize(edit.fileName);
    if (!sources.has(name)) throw new Error("unexpected edit endpoint: " + name);
    groups.set(name, [...(groups.get(name) ?? []), ...edit.textChanges]);
  }
  let patch = "*** Begin Patch\n";
  for (const { name, expected } of sources.values())
    patch += "*** Guard File: " + name + "\n*** Expect: sha256:" + expected + "\n";
  if (destination) {
    patch += "*** Guard File: " + value + "\n*** Expect: absent\n";
    if (!groups.has(selected)) groups.set(selected, []);
  }
  for (const [full, changes] of groups) {
    const { name, text, expected } = sources.get(full);
    let result = text, boundary = text.length;
    for (const { span, newText } of changes.sort((a, b) => b.span.start - a.span.start)) {
      const end = span.start + span.length;
      if (span.start < 0 || end > boundary) throw new Error("overlapping or out-of-bounds language-service edits");
      result = result.slice(0, span.start) + newText + result.slice(end);
      boundary = span.start;
    }
    if (ts.createSourceFile(name, result, ts.ScriptTarget.Latest, false, ts.ScriptKind.TS).parseDiagnostics.length)
      throw new Error("language-service result did not parse: " + name);
    // Native move uses original path bytes, then removes it. A moved source
    // with edits needs an ordinary line hunk (codex-replace is update-only).
    patch += "*** Update File: " + name + "\n";
    if (full === selected && destination) {
      if (changes.length) throw new Error("moves that require rewriting the moved source are not supported");
      patch += "*** Move to: " + value + "\n";
    } else {
      const chunk = (before, after, count = 1) =>
        "@@ codex-replace " + count + " sha256:" + expected + "\n-" +
        JSON.stringify(before) + "\n+" + JSON.stringify(after) + "\n";
      const options = [chunk(text, result)];
      const selected = changes.filter(({ span, newText }) =>
        text.slice(span.start, span.start + span.length) !== newText).reverse();
      if (selected.length) {
        const start = selected[0].span.start;
        const last = selected.at(-1).span;
        const end = last.start + last.length;
        const old = text.slice(start, end);
        const after = result.slice(start, result.length - (text.length - end));
        const utf8 = s => Buffer.from(s).toString("utf8") === s;
        if (old && utf8(old) && utf8(after) && text.indexOf(old) === start &&
            text.indexOf(old, end) === -1) options.push(chunk(old, after));
        // Repeated literals may include comments or unrelated symbols: the
        // native nonoverlapping matches must be exactly the service's spans.
        const groups = new Map();
        for (const { span, newText } of selected) {
          const before = text.slice(span.start, span.start + span.length);
          if (!before || !utf8(before) || !utf8(newText)) break;
          const key = JSON.stringify([before, newText]);
          if (!groups.has(key)) groups.set(key, { before, newText, starts: [] });
          groups.get(key).starts.push(span.start);
        }
        const local = [];
        let represented = 0;
        for (const { before, newText, starts } of groups.values()) {
          let cursor = 0;
          if (!starts.every(start => {
            if (text.indexOf(before, cursor) !== start) return false;
            cursor = start + before.length;
            return true;
          }) || text.indexOf(before, cursor) !== -1) break;
          local.push(chunk(before, newText, starts.length));
          represented += starts.length;
        }
        if (represented === selected.length) options.push(local.join(""));
      }
      patch += options.reduce((best, value) =>
        Buffer.byteLength(value) < Buffer.byteLength(best) ? value : best);
    }
  }
  return patch + "*** End Patch\n";
}

try {
  const [spec, entry] = process.argv.slice(2);
  if (!spec) throw new Error("usage: node typescript_patch.cjs SPEC_JSON [trusted TypeScript module]");
  const ts = require(entry ? path.resolve(entry) : require.resolve("typescript", { paths: [process.cwd()] }));
  process.stdout.write(preview(JSON.parse(spec), ts));
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
