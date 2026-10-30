//! Model-only serialization of unmodified tool return objects. Weak keys avoid
//! retaining results after JavaScript releases them. Trusted display recipes
//! travel only with their values through the existing store/load path.
use serde_json::Value;

use super::RuntimeState;
use super::value::json_to_v8;

const PROJECTOR: &str = r#"((isProxy, boxedKind, projectTool) => {
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const rawJSON = JSON.rawJSON;
  const isRawJSON = JSON.isRawJSON;
  const NativeString = String;
  const booleanValue = Function.prototype.call.bind(Boolean.prototype.valueOf);
  const isArray = Array.isArray;
  const charCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
  const NativeError = Error;
  const displayReplacer = (_key, item) =>
    typeof item === 'bigint' ? {$bigint: item.toString()} : item;
  const displayStringify = value => stringify(value, displayReplacer);
  const toBigInt = BigInt;
  const isSafeInteger = Number.isSafeInteger;
  // Unsafe integer JSON lexemes require at least 16 decimal digits. A cheap
  // conservative scan lets ordinary tool packets use native parsing without
  // visiting every property in a reviver. Digits inside strings only cause a
  // false positive and retain the exact-integer path.
  const possibleUnsafeInteger = RegExp.prototype.exec.bind(/[0-9]{16}/);
  const integerLexeme = RegExp.prototype.exec.bind(/^-?[0-9]+$/);
  const parseExact = value => possibleUnsafeInteger(value) === null ? parse(value) :
    parse(value, (_key, item, context) =>
      typeof item === 'number' && !isSafeInteger(item) &&
      integerLexeme(context.source) !== null ? toBigInt(context.source) : item);
  const keys = Object.getOwnPropertyNames;
  const descriptor = Object.getOwnPropertyDescriptor;
  const prototype = Object.getPrototypeOf;
  const objectPrototype = Object.prototype;
  const arrayPrototype = Array.prototype;
  const projections = new WeakMap();
  const lookup = projections.get.bind(projections);
  const set = projections.set.bind(projections);
  const get = value => {
    const entry = lookup(value);
    if (!entry || entry.disabled) return undefined;
    if (!entry.lazy && !entry.recipeRoot) return entry;
    const root = entry.recipeRoot || entry;
    if (root.lazy) {
      root.projected = projectTool(root.serialized, root.namespace, root.toolName);
      root.disabled = root.projected === undefined;
      if (!root.disabled) root.original = parseExact(root.serialized);
      root.lazy = false;
      delete root.serialized;
    }
    if (root.disabled) { entry.disabled = true; return undefined; }
    if (entry.recipeRoot) {
      let original = root.original, projected = root.projected;
      for (const key of entry.path) {
        original = original?.[key];
        projected = projected?.[key];
      }
      if (projected === undefined) { entry.disabled = true; return undefined; }
      entry.original = original;
      entry.projected = projected;
      entry.rootOriginal = root.original;
      delete entry.recipeRoot;
      delete entry.path;
    }
    return entry;
  };
  // Canonical helper evidence is not a diagnostic preview. Clone data
  // descriptors without invoking user code, retaining every node and control
  // field. Storage's existing byte admission rejects an oversized snapshot
  // before changing a key; display overflow uses canonical cell-output custody.
  // Only uncaught-graph recovery supplies an omission counter. Ordinary
  // canonical transport/storage remains strict and never silently drops data.
  function canonicalError(error, display = false, omissions) {
    const active = new Set();
    const completed = new WeakMap();
    const unsupported = message => {
      if (!omissions) throw new TypeError(message);
      ++omissions.count;
      const marker = Object.create(null);
      marker.code_mode_evidence_omitted = true;
      marker.reason = message;
      return marker;
    };
    function clone(value, depth = 0) {
      if (typeof value === 'function' || typeof value === 'symbol')
        return unsupported('Unsupported canonical error evidence value');
      if (value === null || typeof value !== 'object') return value;
      if (depth >= 128 || isProxy(value) || active.has(value))
        return unsupported('Canonical error evidence is cyclic, proxied, or too deep');
      if (!display && completed.has(value)) return completed.get(value);
      const isError = value instanceof NativeError;
      // Diagnostic causes can be circular or arbitrarily deep. Preserve the
      // graph's complete node/value map while keeping those diagnostics bounded.
      // Canonical transport/storage never accepts this lossy fallback.
      if (display && depth > 0 && isError && !descriptor(value, 'results'))
        return errorProjection(value);
      // Nested graph errors share this clone's active/depth guards. Restarting
      // canonicalError here allowed mutually nested graphs to recurse forever.
      const proto = prototype(value);
      if (!isError && proto !== null && proto !== objectPrototype && proto !== arrayPrototype)
        return unsupported('Canonical error evidence must contain plain data');
      active.add(value);
      try {
        const array = Array.isArray(value);
        const result = array ? [] : Object.create(null);
        if (array) result.length = descriptor(value, 'length').value;
        const names = isError ? [...new Set(['name', 'message', 'cause', 'evidence', 'results',
          ...keys(value).filter(key => key !== 'stack')])] : keys(value);
        for (const key of names) {
          if (array && key === 'length') continue;
          let field = descriptor(value, key);
          if (!field && isError && key === 'name') field = descriptor(proto, key);
          if (!field) continue;
          const content = 'value' in field ? clone(field.value, depth + 1) :
            unsupported('Canonical error evidence cannot contain accessors');
          Object.defineProperty(result, key, {value: content,
            enumerable: true, writable: true, configurable: true});
        }
        if (!display) completed.set(value, result);
        return result;
      } finally { active.delete(value); }
    }
    return clone(error);
  }
  // Uncaught graphs need failure isolation, not the global body-stripping
  // diagnostic preview. Encode each native node/value independently. Only an
  // unsupported field receives an explicit marker; exact siblings stay exact.
  function graphEvidence(value, maxBytes, encode) {
    if (!value || typeof value !== 'object' || isProxy(value)) return null;
    const omitted = reason => '{"code_mode_evidence_omitted":true,"reason":' + stringify(reason) + '}';
    const attempt = (item, key) => {
      try { return {json: encode(item, key)}; }
      catch { return {failed: true}; }
    };
    const rows = [];
    for (const id of keys(value)) {
      const node = descriptor(value, id);
      let encoded;
      if (node && 'value' in node && node.value && typeof node.value === 'object' &&
          !isProxy(node.value) && [null, objectPrototype].includes(prototype(node.value))) {
        // Native graph records are descriptor-only wrappers. Encode each field
        // once: a later failed reason must not re-run a successful value's
        // serializer or discard its already-encoded receipt.
        const fields = [], incomplete = [];
        let omissionCount = 0;
        for (const key of keys(node.value)) {
          const field = descriptor(node.value, key);
          let part = field && 'value' in field ? attempt(field.value, key) : {failed: true};
          if (part.failed && field && 'value' in field) {
            const omissions = {count: 0};
            try { part = attempt(canonicalError(field.value, false, omissions), key); }
            catch { part = {failed: true}; }
            omissionCount += omissions.count;
            if (omissions.count) incomplete.push(key);
          }
          if (typeof part.json !== 'string') {
            part.json = omitted(part.failed ? 'unserializable_field' : 'canonical_byte_limit');
            ++omissionCount;
            incomplete.push(key);
          }
          fields.push(stringify(key) + ':' + part.json);
        }
        if (incomplete.length) {
          fields.push('"_codex_evidence":{"complete":false,"incomplete_fields":[' +
            incomplete.map(key => stringify(key)).join(',') + '],"omission_count":' + omissionCount + '}');
        }
        encoded = {json: '{' + fields.join(',') + '}'};
      } else {
        encoded = node && 'value' in node ? attempt(node.value, id) : {failed: true};
        if (typeof encoded.json !== 'string')
          encoded.json = '{"_codex_evidence":{"complete":false},"value":' + omitted('unsupported_node') + '}';
      }
      rows.push(stringify(id) + ':' + encoded.json);
    }
    const json = '{' + rows.join(',') + '}';
    return utf8Size(json) <= maxBytes ? json : null;
  }
  let canonicalSerialization = false;
  // Display-only: retain native errors in scripts, but never print opaque {}.
  function errorProjection(error, remaining = 16384, controlsFirst = true) {
    const seen = new Set();
    function bounded(value, depth = 0, settledNodes = false) {
      if (remaining <= 0 || depth >= 8) return "[error details truncated]";
      if (typeof value === 'string') {
        const count = Math.min(4096, remaining);
        remaining -= Math.min(value.length, count);
        return value.length > count ? value.slice(0, count) + "[truncated]" : value;
      }
      if (value === null || typeof value !== 'object') return value;
      if (seen.has(value)) return "[circular error evidence]";
      seen.add(value);
      const array = Array.isArray(value);
      const result = Object.create(null);
      const fields = array ? (result.entries = Object.create(null)) : result;
      let names;
      try {
        if (array) result.original_length = descriptor(value, 'length')?.value;
        names = value instanceof NativeError ? [...new Set(['name', 'message', 'cause', 'evidence', 'results',
          ...keys(value).filter(key => key !== 'stack')])] :
          keys(value).filter(key => !(array && key === 'length'));
      } catch { return '[error evidence unavailable]'; }
      if (controlsFirst) {
        const controls = ['tool_failure', 'code', 'effects', 'retry_safe', 'safe_next_actions', 'live_operations',
          'status', 'artifact_id', 'session_id', 'recovery', 'continuation',
          'complete', 'execution_state', 'exit_code', 'process_exited', 'session_capabilities',
          'name', 'message', 'reason', 'step_id', 'terminal', 'value', 'initial', 'observations',
          'blocked_by', 'missing_capabilities', 'omitted_capabilities'];
        names.sort((a, b) => (a === 'tool_failure' ? -1 : Number(!controls.includes(a))) -
          (b === 'tool_failure' ? -1 : Number(!controls.includes(b))));
      }
      const selectedNames = names.slice(0, 64);
      for (const [index, key] of selectedNames.entries()) {
        if (remaining <= 0) { result.details_omitted = true; break; }
        if (key.length > 512 || key.length + 8 > remaining) { result.details_omitted = true; continue; }
        remaining -= key.length + 8;
        // Diagnostic getters must not prevent successful siblings from printing.
        try {
          let field = descriptor(value, key);
          if (!field && value instanceof NativeError && key === 'name') {
            field = descriptor(prototype(value), key);
          }
          if (field) {
            // A caught graph Error goes through toJSON before the replacer.
            // Partition its existing budget instead of letting an early body
            // hide later settled statuses and live handles.
            const available = remaining;
            if (settledNodes) remaining = Math.floor(remaining / (selectedNames.length - index));
            const allowance = remaining;
            fields[key] = 'value' in field ? bounded(field.value, depth + 1,
              value instanceof NativeError && key === 'results') : '[accessor omitted]';
            if (settledNodes) remaining = available - (allowance - remaining);
          }
        } catch { fields[key] = '[error evidence unavailable]'; }
      }
      if (names.length > 64) result.details_omitted = true;
      return result;
    }
    return bounded(error);
  }
  // Helper envelopes must remain actionable at the full legal graph capacity.
  // Do not let a diagnostic property/string budget become persisted evidence.
  // Ordinary errors still use bounded display; all originals remain untouched.
  Object.defineProperty(NativeError.prototype, 'toJSON', {
    value: function toJSON() {
      if (canonicalSerialization) return canonicalError(this);
      return descriptor(this, 'results') ? canonicalError(this, true) : errorProjection(this);
    },
    writable: true, configurable: true,
  });
  function same(value, original, depth = 0) {
    if (value === original) return true;
    if (depth >= 128 || value === null || original === null ||
        typeof value !== 'object' || typeof original !== 'object') return false;
    // Descriptor equality cannot prove serialization equivalence for a Proxy.
    // Check before any prototype/descriptor operation could invoke its traps.
    if (isProxy(value) || isProxy(original)) return false;
    if (prototype(value) !== prototype(original)) return false;
    const names = keys(original);
    if (keys(value).length !== names.length) return false;
    for (const name of names) {
      const field = descriptor(value, name);
      const source = descriptor(original, name);
      // Never evaluate accessors a second time or replace a changed result.
      if (!field || !('value' in field) || field.enumerable !== source.enumerable ||
          !same(field.value, source.value, depth + 1)) return false;
    }
    return true;
  }
  // Root memoization is safe only if this serialization cannot execute user
  // code between rows. Inspect data descriptors without calling getters or
  // proxy traps, including roots referenced only by selected fragments.
  function inert(value, seen, depth = 0) {
    if (typeof value === 'function') return false;
    if (value === null || typeof value !== 'object') return true;
    if (depth >= 128 || isProxy(value)) return false;
    if (seen.has(value)) return true;
    seen.add(value);
    const proto = prototype(value);
    if (proto !== null && proto !== objectPrototype && proto !== arrayPrototype) return false;
    if (prototype(objectPrototype) !== null || prototype(arrayPrototype) !== objectPrototype ||
        descriptor(objectPrototype, 'toJSON') || descriptor(arrayPrototype, 'toJSON')) return false;
    for (const name of keys(value)) {
      const field = descriptor(value, name);
      if (!field || !('value' in field) || !inert(field.value, seen, depth + 1)) return false;
    }
    const entry = get(value);
    return !entry?.root || inert(entry.root, seen, depth + 1);
  }
  // Native escaping won a narrow ASCII probe; this predicate instead avoids
  // allocating that full escaped copy. Match JSON's lone-surrogate rules.
  const escapes = text => {
    for (let i = 0; i < text.length; ++i) {
      const cp = charCodeAt(text, i);
      if (cp < 32 || cp === 34 || cp === 92) return true;
      if (cp >= 0xd800 && cp <= 0xdfff) {
        const next = charCodeAt(text, i + 1);
        if (cp > 0xdbff || !(next >= 0xdc00 && next <= 0xdfff)) return true;
        ++i;
      }
    }
    return false;
  };
  const stringIndexOf = Function.prototype.call.bind(String.prototype.indexOf);
  const stringSlice = Function.prototype.call.bind(String.prototype.slice);
  const stringCodePointAt = Function.prototype.call.bind(String.prototype.codePointAt);
  const lineCount = text => {
    let count = 1, offset = -1;
    while ((offset = stringIndexOf(text, '\n', offset + 1)) !== -1) ++count;
    return count;
  };
  const utf8Size = text => {
    let size = 0;
    for (let i = 0; i < text.length; ++i) {
      const cp = stringCodePointAt(text, i);
      size += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
      if (cp > 0xffff) ++i;
    }
    return size;
  };
  function displaySize(value, utf8 = false) {
    let stringSize = 0;
    // Keep native traversal, user serializers and BigInt display conversion.
    // Count primitive string contents, leaving only empty strings in the
    // temporary JSON skeleton instead of materializing escaped source bodies.
    const skeleton = stringify(value, (key, item) => {
      item = displayReplacer(key, item);
      if (typeof item !== 'string') return item;
      for (let i = 0; i < item.length; ++i) {
        const cp = charCodeAt(item, i);
        if (cp === 34 || cp === 92) stringSize += 2;
        else if (cp < 32)
          stringSize += cp === 8 || cp === 9 || cp === 10 || cp === 12 || cp === 13 ? 2 : 6;
        else if (cp >= 0xd800 && cp <= 0xdfff) {
          const next = charCodeAt(item, i + 1);
          if (cp <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
            stringSize += utf8 ? 4 : 2;
            ++i;
          } else stringSize += 6;
        } else stringSize += utf8 ? cp < 0x80 ? 1 : cp < 0x800 ? 2 : 3 : 1;
      }
      return '';
    });
    return stringSize + (utf8 ? utf8Size(skeleton) : skeleton.length);
  }
  // Shared hydration is scoped to its parent response. A selected row may not
  // contain the earlier body, so resolve it from the retained immutable parent.
  function standaloneFragment(fragment, source, cache) {
    const bodies = value => {
      if (Array.isArray(value)) return value.flatMap(bodies);
      if (!value || typeof value !== 'object') return [];
      if (value.canonical_range && (typeof value.text === 'string' || value.data_base64)) return [value];
      if (Array.isArray(value.results)) return bodies(value.results);
      return value.selector?.kind === 'search' && value.status === 'ok'
        ? bodies(value.value?.hydrated_ranges || []) : [];
    };
    const local = bodies(fragment);
    let cached = cache?.get(source);
    if (cache && !cached) {
      cached = {bodies: bodies(source), ranges: new Map(), positions: new Map()};
      cache.set(source, cached);
    }
    const retained = cached?.bodies || bodies(source);
    const covers = (body, range) => body.canonical_range.start <= range.start && body.canonical_range.end >= range.end;
    const resolve = item => {
      if (Array.isArray(item)) return item.map(resolve);
      if (!item || typeof item !== 'object') return item;
      if (item.shared === true && item.canonical_range) {
        const range = item.canonical_range;
        if (local.some(body => covers(body, range))) return item;
        const key = range.start + ':' + range.end;
        if (cached?.ranges.has(key)) {
          const {shared, ...rest} = item;
          return {...rest, text: cached.ranges.get(key)};
        }
        const body = retained.find(body => covers(body, range));
        if (body && typeof body.text === 'string') {
          // Sparse UTF-8 checkpoints belong only to this immutable parent and
          // serialization. Distinct tail ranges must not rescan its full prefix.
          let index = cached?.positions.get(body);
          if (!index) {
            index = {points: [[0, body.canonical_range.start]], chars: 0,
              bytes: body.canonical_range.start};
            cached?.positions.set(body, index);
          }
          const width = cp => cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
          while (index.bytes < range.end && index.chars < body.text.length) {
            const cp = stringCodePointAt(body.text, index.chars);
            index.bytes += width(cp);
            index.chars += cp > 0xffff ? 2 : 1;
            if (index.chars - index.points[index.points.length - 1][0] >= 1024)
              index.points.push([index.chars, index.bytes]);
          }
          let low = 0, high = index.points.length;
          while (low + 1 < high) {
            const mid = (low + high) >>> 1;
            if (index.points[mid][1] <= range.start) low = mid;
            else high = mid;
          }
          let [chars, offset] = index.points[low], start;
          while (offset <= range.end && chars <= body.text.length) {
            if (offset === range.start) start = chars;
            if (offset === range.end && start !== undefined) {
              const text = stringSlice(body.text, start, chars);
              cached?.ranges.set(key, text);
              const {shared, ...rest} = item;
              return {...rest, text};
            }
            if (chars === body.text.length || (offset > range.start && start === undefined)) break;
            const cp = stringCodePointAt(body.text, chars);
            offset += width(cp);
            chars += cp > 0xffff ? 2 : 1;
          }
        }
        return {...item, recovery: {tool: 'read_tool_output',
          source_sha256: source.source_sha256 || source.canonical_sha256,
          canonical_sha256: source.canonical_sha256, source_encoding: source.source_encoding,
          selector_encoding: source.selector_encoding, selector_coordinate_space: source.selector_coordinate_space,
          arguments: {artifact_id: source.artifact_id,
            selectors: [{kind: 'bytes', start: range.start, end: range.end}]}}};
      }
      if (item.selector?.kind === 'search' && item.status === 'ok' && item.value?.hydrated_ranges)
        return {...item, value: {...item.value, hydrated_ranges: resolve(item.value.hydrated_ranges)}};
      return item;
    };
    return resolve(fragment);
  }
  // Registered results, including ones inside batches, keep their JSON shape
  // in a one-line envelope. Counted text bodies follow in traversal order.
  // Only tool-owned text slots qualify, never arbitrary user JSON strings.
  function smallerFrame(projected, raw) {
    // Include all attribution and conservative batch body-index overhead. Raw
    // bodies contribute the same UTF-8 bytes in both forms; escaped JSON and
    // added framing differ only in their surrounding representation.
    const framedLength = displaySize(raw.envelope, true) + raw.texts.reduce((size, body) =>
      size + displaySize({body: Number.MAX_SAFE_INTEGER, ...body.source}, true) + 11 + utf8Size(body.text), 0);
    return framedLength < displaySize(projected, true) ? raw : undefined;
  }
  function rawText(projected, fragment, source = projected) {
    if (fragment) {
      const raw = rawText({results: fragment === 'rows' ? projected : [projected]}, undefined, source);
      if (!raw) return undefined;
      const rows = fragment === 'rows' ? projected : [projected];
      const unframed = rows.some(row => typeof row?.text !== 'string' &&
        !(row?.selector?.kind === 'search' && row.value?.hydrated_ranges?.some(range => typeof range?.text === 'string')));
      const selected = fragment === 'rows' ? raw.envelope.results : raw.envelope.results[0];
      // Non-text rows have no source body label. Preserve their trusted parent
      // identity in presentation only, after the existing mutation guards.
      const envelope = unframed ? {source: {
        path: source.path, artifact_id: source.artifact_id,
        source_sha256: source.source_sha256 || source.canonical_sha256,
        canonical_sha256: source.canonical_sha256, source_encoding: source.source_encoding,
        selector_encoding: source.selector_encoding, selector_coordinate_space: source.selector_coordinate_space,
        environment_id: source.environment_id,
        canonical_uri: source.path === undefined ? source.canonical_uri : undefined,
      }, [fragment === 'rows' ? 'results' : 'result']: selected} : selected;
      return {envelope, texts: raw.texts};
    }
    if (projected === null || typeof projected !== 'object' || Array.isArray(projected)) {
      return undefined;
    }
    if (typeof projected.output === 'string' && !('results' in projected)) {
      const {output, ...envelope} = projected;
      if (!escapes(output)) return undefined;
      const text_lines = lineCount(output);
      envelope.output_lines = text_lines;
      return smallerFrame(projected, {envelope, texts: [{text: output, source: {
        path: source.path, artifact_id: source.artifact_id,
        session_id: source.session_id, chunk_id: source.chunk_id, field: 'output',
        text_lines,
      }}]});
    }
    if (Array.isArray(projected.results) && !('output' in projected)) {
      const texts = [];
      const frame = (item) => {
        if (item === null || typeof item !== 'object' || typeof item.text !== 'string') {
          return item;
        }
        const {text, ...rest} = item;
        const text_lines = lineCount(text);
        // A whole result's envelope already shows its path and artifact; a
        // selected fragment prints without one, so only its label names them.
        // Only an incomplete body needs its completeness repeated here.
        texts.push({text, source: {
          ...(source !== projected ? {path: source.path, artifact_id: source.artifact_id,
            source_sha256: source.source_sha256 || source.canonical_sha256,
            canonical_sha256: source.canonical_sha256, source_encoding: source.source_encoding,
            selector_encoding: source.selector_encoding, selector_coordinate_space: source.selector_coordinate_space,
            environment_id: source.environment_id, canonical_uri: source.canonical_uri} : {}),
          selector: item.selector, canonical_range: item.canonical_range,
          complete: item.complete === true ? undefined : item.complete, text_lines}});
        return {...rest, text_lines};
      };
      const results = projected.results.map((result) => {
        const framed = frame(result);
        if (result?.selector?.kind === 'search' &&
            Array.isArray(result.value?.hydrated_ranges)) {
          return {...framed, value: {...result.value,
            hydrated_ranges: result.value.hydrated_ranges.map(frame)}};
        }
        return framed;
      });
      // Standalone registered rows have no parent envelope. Even a short or
      // empty body needs its trusted source label, not just its text/range.
      if (!texts.some(body => escapes(body.text)) && source === projected) return undefined;
      const raw = {envelope: {...projected, results}, texts};
      return source === projected ? smallerFrame(projected, raw) : raw;
    }
    if (Array.isArray(projected.content)) {
      const texts = [];
      const content = projected.content.map((item, index) => {
        if (item?.type !== 'text' || typeof item.text !== 'string' || 'text_lines' in item) return item;
        const {text, ...rest} = item;
        const text_lines = lineCount(text);
        texts.push({text, source: {field: `content[${index}].text`, text_lines}});
        return {...rest, text_lines};
      });
      if (!texts.some(body => escapes(body.text))) return undefined;
      const envelope = {...projected, content};
      // Short MCP messages should not grow just to acquire a frame. Budget
      // conservatively for body indices when this result is nested in a batch.
      const framedLength = displaySize(envelope) + texts.reduce((size, body) =>
        size + displaySize({body: Number.MAX_SAFE_INTEGER, ...body.source}) + 11 + body.text.length, 0);
      if (framedLength >= displaySize(projected)) return undefined;
      return {envelope, texts};
    }
    return undefined;
  }
  const budgetExceeded = new NativeError('Canonical JSON byte budget exceeded');
  function canonicalReplacer(maxBytes) {
    let remaining = maxBytes, root = true;
    const parents = [];
    // A batch can reference the same immutable body repeatedly. Cache only
    // successfully counted strings, within this traversal and a small bound;
    // every occurrence still spends its full encoded byte cost.
    let stringCosts;
    let cachedChars = 0;
    const take = bytes => {
      if (bytes > remaining) throw budgetExceeded;
      remaining -= bytes;
    };
    const stringBytes = (text, quoted) => {
      const cached = quoted && text.length >= 512 ? stringCosts?.get(text) : undefined;
      if (cached !== undefined) { take(cached); return; }
      const before = remaining;
      if (quoted) take(2);
      // Every UTF-16 code unit needs at least one encoded byte. Reject a giant
      // primitive before allocating another escaped copy of it.
      if (text.length > remaining) throw budgetExceeded;
      for (let i = 0; i < text.length; ++i) {
        const cp = charCodeAt(text, i);
        if (quoted && (cp === 34 || cp === 92)) take(2);
        else if (quoted && cp < 32)
          take([8, 9, 10, 12, 13].includes(cp) ? 2 : 6);
        else if (cp < 128) take(1);
        else if (cp < 2048) take(2);
        else if (cp >= 0xd800 && cp <= 0xdfff) {
          const next = charCodeAt(text, i + 1);
          if (cp <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
            take(4); ++i;
          } else take(quoted ? 6 : 3);
        } else take(3);
      }
      if (quoted && text.length >= 512 && (stringCosts?.size || 0) < 256 &&
          cachedChars + text.length <= 4 * 1024 * 1024) {
        (stringCosts ??= new Map()).set(text, before - remaining);
        cachedChars += text.length;
      }
    };
    return function bounded(key, item) {
      if (typeof item === 'bigint') {
        if (item < -9223372036854775808n || item > 18446744073709551615n)
          throw new RangeError('BigInt exceeds the exact JSON integer transport range');
        item = rawJSON(item.toString());
      }
      // JSON unboxes these after its replacer. Do that once at the same point,
      // so custom coercions are neither repeated nor omitted by accounting.
      if (item !== null && typeof item === 'object') {
        // Native brand checks neither invoke proxy traps nor depend on a
        // wrapper retaining its original prototype.
        const kind = boxedKind(item);
        if (kind === 1) item = +item;
        else if (kind === 2) item = NativeString(item);
        else if (kind === 3) item = booleanValue(item);
      }
      while (parents.length && parents[parents.length - 1].value !== this) parents.pop();
      const parent = parents[parents.length - 1];
      const omitted = item === undefined || typeof item === 'function' || typeof item === 'symbol';
      if (omitted && !parent?.array) { root = false; return item; }
      if (!root && parent) {
        if (parent.count++) take(1);
        if (!parent.array) { stringBytes(key, true); take(1); }
      }
      root = false;
      if (omitted || item === null) take(4);
      else if (typeof item === 'string') stringBytes(item, true);
      else if (typeof item === 'number' || typeof item === 'boolean') take(stringify(item).length);
      else if (isRawJSON(item)) stringBytes(item.rawJSON, false);
      else {
        take(2);
        parents.push({value: item, array: isArray(item), count: 0});
      }
      return item;
    };
  }
  return function project(value, original, projected, sourceFragments) {
    if (arguments.length === 5) {
      // Retain the transport string as an immutable mutation baseline. The
      // extra object graphs and display projection exist only when observed.
      const root = {lazy: true, serialized: original, namespace: projected,
        toolName: sourceFragments, sourceFragments: arguments[4]};
      set(value, root);
      if (root.sourceFragments && Array.isArray(value.results)) {
        const rows = (values, path) => {
          set(values, {recipeRoot: root, path, fragment: 'rows', root: value});
          values.forEach((row, i) => {
            if (row === null || typeof row !== 'object') return;
            const rowPath = [...path, i];
            set(row, {recipeRoot: root, path: rowPath, fragment: 'row', root: value});
            if (row.selector?.kind === 'search' && Array.isArray(row.value?.hydrated_ranges))
              rows(row.value.hydrated_ranges, [...rowPath, 'value', 'hydrated_ranges']);
          });
        };
        rows(value.results, ['results']);
      }
      return;
    }
    // Only the native store/load callbacks can export/import this metadata.
    // Keep a small edit recipe, not a second copy of source text. Arbitrary
    // lookalike JSON never gains tool provenance.
    if (arguments.length === 2 && original === 'capture_presentation') {
      if (!inert(value, new Set())) return [];
      const records = [];
      const delta = (before, after, path = [], edits = []) => {
        if (same(before, after)) return edits;
        if (Array.isArray(before) && Array.isArray(after) && before.length === after.length) {
          for (let index = 0; index < after.length; index++) delta(before[index], after[index], [...path, String(index)], edits);
        } else if (before && after && typeof before === 'object' && typeof after === 'object' &&
            !Array.isArray(before) && !Array.isArray(after)) {
          for (const key of keys(before)) if (!descriptor(after, key)) edits.push({path: [...path, key], remove: true});
          for (const key of keys(after)) delta(descriptor(before, key)?.value, after[key], [...path, key], edits);
        } else edits.push({path, value: after});
        return edits;
      };
      const visit = (item, path, depth) => {
        if (!item || typeof item !== 'object' || depth >= 128) return;
        const entry = get(item);
        if (entry && same(item, entry.original) && (!entry.root || same(entry.root, entry.rootOriginal))) {
          const display = entry.fragment ? standaloneFragment(entry.projected, entry.rootOriginal) : entry.projected;
          const source = entry.rootOriginal;
          records.push({path, edits: delta(item, display), fragment: entry.fragment,
            sourceFragments: entry.sourceFragments,
            source: source && {path: source.path, artifact_id: source.artifact_id,
              source_sha256: source.source_sha256, canonical_sha256: source.canonical_sha256,
              source_encoding: source.source_encoding, selector_encoding: source.selector_encoding,
              selector_coordinate_space: source.selector_coordinate_space,
              environment_id: source.environment_id, canonical_uri: source.canonical_uri}});
          return;
        }
        for (const key of keys(item)) {
          const field = descriptor(item, key);
          if (field.enumerable) visit(field.value, [...path, key], depth + 1);
        }
      };
      visit(value, [], 0);
      return records;
    }
    if (arguments.length === 3 && original === 'restore_presentation') {
      const clone = item => project(project(item, 'serialize'), 'parse');
      for (const record of projected) {
        let target = value;
        for (const key of record.path) target = descriptor(target, key)?.value;
        if (!target || typeof target !== 'object') continue;
        let display = clone(target);
        for (const edit of record.edits) {
          if (!edit.path.length) { display = edit.value; continue; }
          let parent = display;
          for (const key of edit.path.slice(0, -1)) parent = descriptor(parent, key)?.value;
          const key = edit.path[edit.path.length - 1];
          if (edit.remove) delete parent[key];
          else Object.defineProperty(parent, key, {value: edit.value, enumerable: true, writable: true, configurable: true});
        }
        if (record.fragment) {
          // Reuse native source registration, including selectable descendants.
          // The private envelope also guards mutations of the restored parent.
          const root = {...record.source, results: record.fragment === 'rows' ? target : [target]};
          const shown = {...record.source, results: record.fragment === 'rows' ? display : [display]};
          project(root, clone(root), shown, true);
        } else project(target, clone(target), display, record.sourceFragments);
      }
      return;
    }
    // The transport codec shares this cell-owned, captured serializer, not
    // script-overridable JSON methods. BigInt never silently becomes Number.
    if (arguments.length === 3 && original === 'graph_evidence') {
      return graphEvidence(value, projected, (item, key) => {
        // Preserve toJSON's original property-key argument while isolating
        // fields. This private wrapper cannot invoke a prototype serializer.
        const wrapper = Object.create(null);
        wrapper[key] = item;
        const json = project(wrapper, 'canonical', projected);
        if (typeof json !== 'string' || json === '{}') return undefined;
        return stringSlice(json, stringify(key).length + 2, -1);
      });
    }
    if (arguments.length === 2 || (arguments.length === 3 && original === 'canonical')) {
      if (original === 'helper_evidence') {
        // Reserve exact controls globally, recursively, before optional body
        // previews. A coordinating node must not divide its leaves' allowance.
        let remaining = 1024 * 1024, items = 0;
        const active = new Set();
        const body = new Set(['output', 'stdout', 'stderr', 'text', 'content', 'body', 'stack']);
        const controls = new Set(['status', 'results', 'reason', 'value', 'evidence',
          'receipt', 'recovery', 'continuation', 'artifact_id', 'session_id',
          'session_capabilities', 'incarnation', 'terminal', 'observations']);
        const exact = (item, depth = 0) => {
          if (++items > 16384 || depth >= 64 || remaining < 32) return {details_omitted:true};
          if (item === null || typeof item !== 'object') {
            const size = typeof item === 'string' ? stringify(item).length * 3 : 32;
            if (size > remaining) return {details_omitted:true};
            remaining -= size;
            return typeof item === 'bigint' ? {$bigint:item.toString()} : item;
          }
          if (isProxy(item) || active.has(item)) return {details_omitted:true};
          active.add(item);
          try {
            const result = Object.create(null);
            const names = keys(item).filter(key => key !== 'length' && !body.has(key));
            names.sort((a,b) => Number(!controls.has(a)) - Number(!controls.has(b)));
            for (const key of names) {
              if (key.length > 512 || remaining < key.length * 6 + 32) { result.details_omitted = true; continue; }
              remaining -= key.length * 6 + 8;
              const field = descriptor(item, key);
              result[key] = field && 'value' in field ? exact(field.value, depth + 1) : '[accessor omitted]';
            }
            if (keys(item).some(key => body.has(key))) result.body_omitted = true;
            return result;
          } finally { active.delete(item); }
        };
        const names = value !== null && typeof value === 'object' ? keys(value) : [];
        const entries = names.filter(name => name !== 'length').slice(0, 256).map(name => {
          const field = descriptor(value, name);
          return {key: name.length > 512 ? '[node identity omitted]' : name,
            value: field && 'value' in field ? exact(field.value) : '[accessor omitted]'};
        });
        return stringify({bounded_helper_evidence: true, entries,
          omitted_entries: Math.max(0, names.filter(name => name !== 'length').length - entries.length)},
          (_key, item) => typeof item === 'bigint' ? {$bigint:item.toString()} : item);
      }
      if (original === 'parse') {
        return parseExact(value);
      }
      const previous = canonicalSerialization;
      if (original === 'canonical') canonicalSerialization = true;
      try {
        if (arguments.length === 3 && original === 'canonical') {
          try { return stringify(value, canonicalReplacer(projected)); }
          catch (error) { if (error === budgetExceeded) return null; throw error; }
        }
        return stringify(value, (_key, item) => {
          if (typeof item !== 'bigint') return item;
          if (item < -9223372036854775808n || item > 18446744073709551615n)
            throw new RangeError('BigInt exceeds the exact JSON integer transport range');
          return rawJSON(item.toString());
        });
      } finally { canonicalSerialization = previous; }
    }
    if (arguments.length === 4) {
      set(value, {original, projected, sourceFragments});
      // Selected native source rows should not need the entire envelope just
      // to avoid JSON-escaping their text. Keep identity and mutation guards;
      // arbitrary JSON and unregistered clones remain untouched.
      if (sourceFragments && Array.isArray(value.results) && Array.isArray(projected.results)) {
        const rows = (values, originals, displays) => {
          set(values, {original: originals, projected: displays, fragment: 'rows', root: value, rootOriginal: original});
          values.forEach((row, i) => {
            if (row === null || typeof row !== 'object' || !displays[i]) return;
            set(row, {original: originals[i], projected: displays[i], fragment: 'row', root: value, rootOriginal: original});
            if (row.selector?.kind === 'search' && Array.isArray(row.value?.hydrated_ranges)) {
              rows(row.value.hydrated_ranges, originals[i].value.hydrated_ranges, displays[i].value.hydrated_ranges);
            }
          });
        };
        rows(value.results, original.results, projected.results);
      }
      return;
    }
    // Display uses its own budget while retaining its projection/framing
    // semantics. Charge within the same native traversal: never replay getters
    // or toJSON merely to estimate the eventual representation's size.
    const displayLimit = arguments.length === 3 && original === 'display' ? projected : undefined;
    const render = (item, replace = displayReplacer) => {
      if (displayLimit === undefined) return stringify(item, replace);
      const charge = canonicalReplacer(displayLimit);
      return stringify(item, function(key, value) {
        return charge.call(this, key, replace(key, value));
      });
    };
    try {
    // A small unchanged command has no source body to frame or share.
    const direct = value !== null && typeof value === 'object' ? get(value) : undefined;
    if (direct && !direct.root && typeof direct.projected?.output === 'string' &&
        !escapes(direct.projected.output) && inert(value, new Set()) && same(value, direct.original)) {
      return render(direct.projected);
    }
    const texts = [];
    // Both caches die with this serialization. No cross-call provenance,
    // freshness, or mutation decisions are cached.
    let fragmentCache, framedBodies;
    const equalBodies = new Map();
    let internedChars = 0;
    const append = (raw, owner) => {
      if (raw.texts.every(body => body.text.length < 512)) {
        texts.push(...raw.texts);
        return;
      }
      framedBodies ??= new Map();
      let previous = framedBodies.get(owner);
      if (!previous) framedBodies.set(owner, previous = []);
      raw.texts.forEach((body, index) => {
        const first = previous[index] ?? (roots ? equalBodies.get(body.text) : undefined);
        // Exact string-key equality shares rendering, never source identity.
        // Content interning is restricted to inert packets and bounded below;
        // all source labels and the existing mutation opt-outs remain intact.
        if (first !== undefined && body.text.length >= 512 && texts[first].text === body.text) {
          texts.push({...body, same_as_body: first + 1});
        } else {
          previous[index] = texts.length;
          if (roots && body.text.length >= 512 && equalBodies.size < 1024 &&
              internedChars + body.text.length <= 4 * 1024 * 1024 &&
              !equalBodies.has(body.text)) {
            equalBodies.set(body.text, texts.length);
            internedChars += body.text.length;
          }
          texts.push(body);
        }
      });
    };
    const roots = inert(value, new Set()) ? new Map() : undefined;
    // No getters, proxies or user serializers can run in an inert batch.
    // Repeated registered objects therefore share framing within this call only.
    const frames = roots ? new Map() : undefined;
    const unchangedRoot = entry => {
      if (!entry.root) return true;
      if (!roots) return same(entry.root, entry.rootOriginal);
      if (!roots.has(entry.root)) roots.set(entry.root, same(entry.root, entry.rootOriginal));
      return roots.get(entry.root);
    };
    const rendered = render(value, (_key, item) => {
      if (typeof item === 'bigint') return displayReplacer(_key, item);
      if (item instanceof NativeError) return errorProjection(item);
      const entry = item !== null && typeof item === 'object' ? get(item) : undefined;
      if (!entry) return item;
      if (frames?.has(item)) {
        const {raw, display} = frames.get(item);
        if (raw === undefined) return display;
        append(raw, entry.original);
        return raw.envelope;
      }
      if (!same(item, entry.original)) {
        // Helpers annotate wrappers. For caller-added command metadata, keep
        // the command projection only if every original field is unchanged.
        // Accessors, proxies, custom serialization and evidence edits opt out.
        if (entry.root || typeof entry.original.output !== 'string' ||
            !inert(item, new Set()) || prototype(item) !== prototype(entry.original)) return item;
        const extras = keys(item).filter(name => !descriptor(entry.original, name));
        // Never overwrite caller additions with synthesized display fields.
        if (extras.some(name => descriptor(entry.projected, name) || name === 'output_lines')) return item;
        if (!extras.length || !keys(entry.original).every(name => {
          const field = descriptor(item, name), source = descriptor(entry.original, name);
          return field && 'value' in field && field.enumerable === source.enumerable && same(field.value, source.value);
        })) return item;
        const annotated = {...entry.projected};
        for (const name of extras) {
          const field = descriptor(item, name);
          if (field.enumerable) Object.defineProperty(annotated, name, field);
        }
        const raw = rawText(annotated);
        if (!raw) return annotated;
        append(raw, entry.original);
        return raw.envelope;
      }
      if (!unchangedRoot(entry)) return item;
      const display = entry.fragment ? standaloneFragment(entry.projected, entry.rootOriginal,
        fragmentCache ??= new Map()) : entry.projected;
      const raw = rawText(display, entry.fragment, entry.rootOriginal || entry.projected);
      frames?.set(item, {raw, display});
      if (raw === undefined) return display;
      append(raw, entry.original);
      return raw.envelope;
    });
    if (!texts.length) return rendered;
    let used = utf8Size(rendered) + 1;
    const outputFrames = [];
    for (const [index, body] of texts.entries()) {
      const label = render({body: index + 1, ...body.source, same_as_body: body.same_as_body});
      const raw = body.same_as_body === undefined ? body.text : '';
      const framingBytes = 10 + utf8Size(label) + (index ? 1 : 0);
      // A raw body already exists. Check its lower bound before scanning it,
      // and the complete frame before allocating any concatenated copy.
      if (displayLimit !== undefined && used + framingBytes + raw.length > displayLimit)
        throw budgetExceeded;
      used += framingBytes + utf8Size(raw);
      if (displayLimit !== undefined && used > displayLimit) throw budgetExceeded;
      outputFrames.push('[source ' + label + ']\n' + raw);
    }
    return rendered + '\n' + outputFrames.join('\n');
    } catch (error) {
      if (displayLimit !== undefined && error === budgetExceeded) return null;
      throw error;
    }
  };
})"#;

fn project_tool_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let serialized = args.get(0).to_rust_string_lossy(scope);
    let raw = match serde_json::from_str::<Value>(&serialized) {
        Ok(raw) => raw,
        Err(error) => {
            super::value::throw_type_error(scope, &format!("invalid projection source: {error}"));
            return;
        }
    };
    let namespace = (!args.get(1).is_null_or_undefined())
        .then(|| args.get(1).to_rust_string_lossy(scope));
    let name = args.get(2).to_rust_string_lossy(scope);
    let tool = codex_protocol::ToolName::new(namespace, name);
    if let Some(projected) = codex_code_mode_protocol::printed_tool_result(&tool, &raw) {
        match json_to_v8(scope, &projected) {
            Some(projected) => retval.set(projected),
            None => super::value::throw_type_error(scope, "failed to materialize output projection"),
        }
    }
}

fn is_proxy_callback(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    retval.set_bool(args.get(0).is_proxy());
}

fn boxed_value_kind_callback(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = args.get(0);
    let kind = if value.is_number_object() {
        1
    } else if value.is_string_object() {
        2
    } else if value.is_boolean_object() {
        3
    } else {
        0
    };
    retval.set_int32(kind);
}

pub(super) fn prepare(
    scope: &mut v8::PinScope<'_, '_>,
) -> Result<v8::Global<v8::Function>, String> {
    let source = v8::String::new(scope, PROJECTOR)
        .ok_or_else(|| "failed to allocate output projector".to_string())?;
    let factory = v8::Script::compile(scope, source, None)
        .and_then(|script| script.run(scope))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or_else(|| "failed to install output projector".to_string())?;
    let is_proxy = v8::Function::new(scope, is_proxy_callback)
        .ok_or_else(|| "failed to install proxy guard".to_string())?;
    let boxed_kind = v8::Function::new(scope, boxed_value_kind_callback)
        .ok_or_else(|| "failed to install JSON wrapper brand checks".to_string())?;
    let project_tool = v8::Function::new(scope, project_tool_callback)
        .ok_or_else(|| "failed to install lazy output projection".to_string())?;
    let receiver = v8::undefined(scope).into();
    let function = factory
        .call(scope, receiver, &[is_proxy.into(), boxed_kind.into(), project_tool.into()])
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or_else(|| "failed to initialize output projector".to_string())?;
    Ok(v8::Global::new(scope, function))
}

pub(super) fn install(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    if scope
        .get_slot::<RuntimeState>()
        .is_some_and(|state| state.output_projector.is_some())
    {
        return Ok(());
    }
    let function = prepare(scope)?;
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.output_projector = Some(function);
    }
    Ok(())
}

pub(super) fn tool_result_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    raw: &Value,
    tool: &codex_protocol::ToolName,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let serialized = serde_json::to_string(raw).map_err(|error| error.to_string())?;
    let json = v8::String::new(scope, &serialized)
        .ok_or_else(|| "failed to allocate tool response".to_string())?;
    let value = json_codec(scope, json.into(), "parse")
        .ok_or_else(|| "failed to parse tool response".to_string())?;
    // Keep this eligibility list aligned with model_visible_tool_result. Other
    // tools need no display recipe or retained transport snapshot at all.
    if !raw.is_object() || (tool.namespace.is_none() && !tool.name.starts_with("mcp__")
        && !matches!(tool.name.as_str(), "update_plan" | "apply_patch" | "read_file"
            | "read_tool_output" | "exec_command" | "write_stdin"))
    {
        return Ok(value);
    }
    let namespace: v8::Local<v8::Value> = match tool.namespace.as_deref() {
        Some(namespace) => v8::String::new(scope, namespace)
            .ok_or_else(|| "failed to allocate projection namespace".to_string())?.into(),
        None => v8::null(scope).into(),
    };
    let name = v8::String::new(scope, &tool.name)
        .ok_or_else(|| "failed to allocate projection tool name".to_string())?;
    let fragments = v8::Boolean::new(scope, tool.namespace.is_none()
        && matches!(tool.name.as_str(), "read_file" | "read_tool_output"));
    let function = scope.get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))
        .ok_or_else(|| "output projector unavailable".to_string())?;
    let receiver = v8::undefined(scope).into();
    function.call(scope, receiver, &[value, json.into(), namespace, name.into(), fragments.into()])
        .ok_or_else(|| "failed to register lazy output projection".to_string())?;
    Ok(value)
}

pub(super) fn register(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    raw: &Value,
    projected: &Value,
    source_fragments: bool,
) -> Result<(), String> {
    let original = json_to_v8(scope, raw)
        .ok_or_else(|| "failed to serialize projection source".to_string())?;
    let projected = json_to_v8(scope, projected)
        .ok_or_else(|| "failed to serialize output projection".to_string())?;
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))
        .ok_or_else(|| "output projector unavailable".to_string())?;
    let receiver = v8::undefined(scope).into();
    let source_fragments = v8::Boolean::new(scope, source_fragments).into();
    function
        .call(
            scope,
            receiver,
            &[value, original, projected, source_fragments],
        )
        .ok_or_else(|| "failed to register output projection".to_string())?;
    Ok(())
}

pub(super) fn stringify<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<v8::Local<'s, v8::String>> {
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    function
        .call(scope, receiver, &[value])
        .and_then(|value| v8::Local::<v8::String>::try_from(value).ok())
}

pub(super) fn stringify_with_limit<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Result<Option<v8::Local<'s, v8::String>>, String> {
    let value = bounded_json_codec_mode(scope, value, max_bytes, "display");
    if value.is_some_and(|value| value.is_null()) {
        return Err(format!("payload exceeds its limit of {max_bytes} bytes"));
    }
    Ok(value.and_then(|value| v8::Local::<v8::String>::try_from(value).ok()))
}

pub(super) fn json_codec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    mode: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    let mode = v8::String::new(scope, mode)?;
    function.call(scope, receiver, &[value, mode.into()])
}

pub(super) fn bounded_json_codec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Option<v8::Local<'s, v8::Value>> {
    bounded_json_codec_mode(scope, value, max_bytes, "canonical")
}

pub(super) fn bounded_graph_json_codec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
) -> Option<v8::Local<'s, v8::Value>> {
    bounded_json_codec_mode(scope, value, max_bytes, "graph_evidence")
}

fn bounded_json_codec_mode<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    max_bytes: usize,
    mode: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let function = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    let mode = v8::String::new(scope, mode)?;
    let limit = v8::Number::new(scope, max_bytes as f64);
    function.call(scope, receiver, &[value, mode.into(), limit.into()])
}

pub(super) fn capture_stored_presentation(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<Value> {
    let metadata = json_codec(scope, value, "capture_presentation")?;
    super::value::v8_value_to_json(scope, metadata)
        .ok()
        .flatten()
        .filter(|value| value.as_array().is_some_and(|entries| !entries.is_empty()))
}

pub(super) fn restore_stored_presentation(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    metadata: &Value,
) -> Option<()> {
    let metadata = json_to_v8(scope, metadata)?;
    let function = scope
        .get_slot::<RuntimeState>()?
        .output_projector
        .as_ref()
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    let mode = v8::String::new(scope, "restore_presentation")?;
    function.call(scope, receiver, &[value, mode.into(), metadata])?;
    Some(())
}
