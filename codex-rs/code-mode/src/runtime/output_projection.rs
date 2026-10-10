//! Model-only serialization of unmodified tool return objects. Weak keys avoid
//! retaining results after JavaScript releases them. Trusted display recipes
//! travel only with their values through the existing store/load path.
use serde_json::Value;

use super::RuntimeState;
use super::value::json_to_v8;

const PROJECTOR: &str = r#"((isProxy) => {
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const rawJSON = JSON.rawJSON;
  const NativeError = Error;
  const toBigInt = BigInt;
  const isSafeInteger = Number.isSafeInteger;
  // Unsafe integer JSON lexemes require at least 16 decimal digits. A cheap
  // conservative scan lets ordinary tool packets use native parsing without
  // visiting every property in a reviver. Digits inside strings only cause a
  // false positive and retain the exact-integer path.
  const possibleUnsafeInteger = RegExp.prototype.exec.bind(/[0-9]{16}/);
  const integerLexeme = RegExp.prototype.exec.bind(/^-?[0-9]+$/);
  const keys = Object.getOwnPropertyNames;
  const descriptor = Object.getOwnPropertyDescriptor;
  const prototype = Object.getPrototypeOf;
  const objectPrototype = Object.prototype;
  const arrayPrototype = Array.prototype;
  const projections = new WeakMap();
  const get = projections.get.bind(projections);
  const set = projections.set.bind(projections);
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
        const controls = ['status', 'artifact_id', 'session_id', 'recovery', 'continuation',
          'complete', 'execution_state', 'exit_code', 'process_exited', 'session_capabilities',
          'name', 'message', 'reason', 'step_id', 'terminal', 'value', 'initial', 'observations',
          'blocked_by', 'missing_capabilities', 'omitted_capabilities'];
        names.sort((a, b) => Number(!controls.includes(a)) - Number(!controls.includes(b)));
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
  // Share one bounded serializer with explicit JSON.stringify as well as text.
  // The Error objects and successful sibling values themselves stay untouched.
  Object.defineProperty(NativeError.prototype, 'toJSON', {
    value: function toJSON() { return errorProjection(this); },
    writable: true, configurable: true,
  });
  function same(value, original, depth = 0) {
    if (value === original) return true;
    if (depth >= 128 || value === null || original === null ||
        typeof value !== 'object' || typeof original !== 'object') return false;
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
  // The captured native serializer exactly detects escaping of primitive strings.
  // Regex lookarounds were slower in the narrow ASCII probe. This also avoids
  // script-overridable RegExp methods without changing Unicode semantics.
  const escapes = text => stringify(text).length !== text.length + 2;
  const stringIndexOf = Function.prototype.call.bind(String.prototype.indexOf);
  const stringSlice = Function.prototype.call.bind(String.prototype.slice);
  const stringCodePointAt = Function.prototype.call.bind(String.prototype.codePointAt);
  const lineCount = text => {
    let count = 1, offset = -1;
    while ((offset = stringIndexOf(text, '\n', offset + 1)) !== -1) ++count;
    return count;
  };
  // Shared hydration is scoped to its parent response. A selected row may not
  // contain the earlier body, so resolve it from the retained immutable parent.
  function standaloneFragment(fragment, source, cache) {
    const bodies = value => {
      if (Array.isArray(value)) return value.flatMap(bodies);
      if (!value || typeof value !== 'object') return [];
      if (value.canonical_range && (typeof value.text === 'string' || value.data_base64)) return [value];
      return bodies(value.results || value.value?.hydrated_ranges || []);
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
        return {...item, recovery: {artifact_id: source.artifact_id,
          source_sha256: source.source_sha256 || source.canonical_sha256,
          selectors: [{kind: 'bytes', start: range.start, end: range.end}]}};
      }
      if (item.value?.hydrated_ranges) return {...item, value: {...item.value, hydrated_ranges: resolve(item.value.hydrated_ranges)}};
      return item;
    };
    return resolve(fragment);
  }
  // Registered results, including ones inside batches, keep their JSON shape
  // in a one-line envelope. Counted text bodies follow in traversal order.
  // Only tool-owned text slots qualify, never arbitrary user JSON strings.
  function rawText(projected, fragment, source = projected) {
    if (fragment) {
      const raw = rawText({results: fragment === 'rows' ? projected : [projected]}, undefined, source);
      if (!raw) return undefined;
      return {envelope: fragment === 'rows' ? raw.envelope.results : raw.envelope.results[0], texts: raw.texts};
    }
    if (projected === null || typeof projected !== 'object' || Array.isArray(projected)) {
      return undefined;
    }
    if (typeof projected.output === 'string' && !('results' in projected)) {
      const {output, ...envelope} = projected;
      if (!escapes(output)) return undefined;
      const text_lines = lineCount(output);
      envelope.output_lines = text_lines;
      return {envelope, texts: [{text: output, source: {
        path: source.path, artifact_id: source.artifact_id,
        session_id: source.session_id, chunk_id: source.chunk_id, field: 'output',
        text_lines,
      }}]};
    }
    if (Array.isArray(projected.results) && !('output' in projected)) {
      const texts = [];
      const frame = (item) => {
        if (item === null || typeof item !== 'object' || typeof item.text !== 'string') {
          return item;
        }
        const {text, ...rest} = item;
        const text_lines = lineCount(text);
        texts.push({text, source: {path: source.path, artifact_id: source.artifact_id,
          ...(source !== projected ? {source_sha256: source.source_sha256 || source.canonical_sha256,
            environment_id: source.environment_id, canonical_uri: source.canonical_uri} : {}),
          selector: item.selector, canonical_range: item.canonical_range,
          complete: item.complete, text_lines}});
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
      if (!texts.some(body => escapes(body.text))) return undefined;
      return {envelope: {...projected, results}, texts};
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
      const framedLength = stringify(envelope).length + texts.reduce((size, body) =>
        size + stringify({body: Number.MAX_SAFE_INTEGER, ...body.source}).length + 11 + body.text.length, 0);
      if (framedLength >= stringify(projected).length) return undefined;
      return {envelope, texts};
    }
    return undefined;
  }
  return function project(value, original, projected, sourceFragments) {
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
        if (record.fragment) set(target, {original: clone(target), projected: display,
          fragment: record.fragment, rootOriginal: record.source});
        else project(target, clone(target), display, record.sourceFragments);
      }
      return;
    }
    // The transport codec shares this cell-owned, captured serializer, not
    // script-overridable JSON methods. BigInt never silently becomes Number.
    if (arguments.length === 2) {
      if (original === 'helper_evidence') {
        // Only used after full uncaught-helper serialization fails or exceeds
        // the cell error limit. Give every settled node its own small budget;
        // an oversized sibling must not hide later statuses and handles.
        const names = value !== null && typeof value === 'object' ? keys(value) : [];
        const entries = names.filter(name => name !== 'length').slice(0, 256).map(name => {
          const field = descriptor(value, name);
          return {key: name.length > 512 ? name.slice(0, 512) + '[truncated]' : name,
            value: field && 'value' in field ? errorProjection(field.value, 2048, true) : '[accessor omitted]'};
        });
        return stringify({bounded_helper_evidence: true, entries,
          omitted_entries: Math.max(0, names.filter(name => name !== 'length').length - entries.length)},
          (_key, item) => typeof item === 'bigint' ? {$bigint:item.toString()} : item);
      }
      if (original === 'parse') {
        if (typeof value === 'string' && possibleUnsafeInteger(value) === null) return parse(value);
        return parse(value, (_key, item, context) =>
          typeof item === 'number' && !isSafeInteger(item) &&
          integerLexeme(context.source) !== null ? toBigInt(context.source) : item);
      }
      return stringify(value, (_key, item) => {
        if (typeof item !== 'bigint') return item;
        if (item < -9223372036854775808n || item > 18446744073709551615n)
          throw new RangeError('BigInt exceeds the exact JSON integer transport range');
        return rawJSON(item.toString());
      });
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
    // A small unchanged command has no source body to frame or share.
    const direct = value !== null && typeof value === 'object' ? get(value) : undefined;
    if (direct && !direct.root && typeof direct.projected?.output === 'string' &&
        !escapes(direct.projected.output) && inert(value, new Set()) && same(value, direct.original)) {
      return stringify(direct.projected, (_key, item) =>
        typeof item === 'bigint' ? {$bigint:item.toString()} : item);
    }
    const texts = [];
    // Both caches die with this serialization. No cross-call provenance,
    // freshness, or mutation decisions are cached.
    let fragmentCache, framedBodies;
    const append = (raw, owner) => {
      if (raw.texts.every(body => body.text.length < 512)) {
        texts.push(...raw.texts);
        return;
      }
      framedBodies ??= new Map();
      let previous = framedBodies.get(owner);
      if (!previous) framedBodies.set(owner, previous = []);
      raw.texts.forEach((body, index) => {
        const first = previous[index];
        // Only repeated presentations of the same registered object qualify.
        // Keep small bodies inline, and retain each observation's source label.
        if (first !== undefined && body.text.length >= 512 && texts[first].text === body.text) {
          texts.push({...body, same_as_body: first + 1});
        } else {
          previous[index] = texts.length;
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
    const rendered = stringify(value, (_key, item) => {
      if (typeof item === 'bigint') return { $bigint: item.toString() };
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
    return texts.length ? rendered + '\n' + texts.map((body, index) =>
      '[source ' + stringify({body: index + 1, ...body.source, same_as_body: body.same_as_body}) + ']\n' +
        (body.same_as_body === undefined ? body.text : '')).join('\n') : rendered;
  };
})"#;

fn is_proxy_callback(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    retval.set_bool(args.get(0).is_proxy());
}

pub(super) fn prepare(scope: &mut v8::PinScope<'_, '_>) -> Result<v8::Global<v8::Function>, String> {
    let source = v8::String::new(scope, PROJECTOR)
        .ok_or_else(|| "failed to allocate output projector".to_string())?;
    let factory = v8::Script::compile(scope, source, None)
        .and_then(|script| script.run(scope))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or_else(|| "failed to install output projector".to_string())?;
    let is_proxy = v8::Function::new(scope, is_proxy_callback)
        .ok_or_else(|| "failed to install proxy guard".to_string())?;
    let receiver = v8::undefined(scope).into();
    let function = factory.call(scope, receiver, &[is_proxy.into()])
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or_else(|| "failed to initialize output projector".to_string())?;
    Ok(v8::Global::new(scope, function))
}

pub(super) fn install(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    if scope.get_slot::<RuntimeState>().is_some_and(|state| state.output_projector.is_some()) {
        return Ok(());
    }
    let function = prepare(scope)?;
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.output_projector = Some(function);
    }
    Ok(())
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
        .call(scope, receiver, &[value, original, projected, source_fragments])
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

pub(super) fn json_codec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    mode: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let function = scope.get_slot::<RuntimeState>()
        .and_then(|state| state.output_projector.as_ref())
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    let mode = v8::String::new(scope, mode)?;
    function.call(scope, receiver, &[value, mode.into()])
}

pub(super) fn capture_stored_presentation(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<Value> {
    let metadata = json_codec(scope, value, "capture_presentation")?;
    super::value::v8_value_to_json(scope, metadata).ok().flatten()
        .filter(|value| value.as_array().is_some_and(|entries| !entries.is_empty()))
}

pub(super) fn restore_stored_presentation(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    metadata: &Value,
) -> Option<()> {
    let metadata = json_to_v8(scope, metadata)?;
    let function = scope.get_slot::<RuntimeState>()?.output_projector.as_ref()
        .map(|function| v8::Local::new(scope, function))?;
    let receiver = v8::undefined(scope).into();
    let mode = v8::String::new(scope, "restore_presentation")?;
    function.call(scope, receiver, &[value, mode.into(), metadata])?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::*;
    use codex_code_mode_protocol::ToolDefinition;
    use serde_json::json;

    #[tokio::test]
    async fn critical_path_escape_and_line_count_preserve_native_results() {
        for output in [
            "unescaped λ😀",
            "quoted \"body\" and \\ slash",
            "λ\r\n😀\n",
            "tab\tcontrol\u{0001}",
            "\u{2028}\u{2029}",
        ] {
            let raw = json!({"output":output,"exit_code":0,"process_exited":true});
            let (events, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None,
                tool_call_id: "critical-path-escapes".into(),
                enabled_tools: vec![ToolDefinition {
                    name: "exec_command".into(),
                    tool_name: ToolName::plain("exec_command"),
                    kind: CodeModeToolKind::Function,
                    description: "".into(),
                    input_schema: None,
                    output_schema: None,
                    default_timeout_ms: None,
                }].into(),
                source: r#"
                    const r = await tools.exec_command({});
                    String.prototype.indexOf = () => { throw Error('reentrant indexOf'); };
                    RegExp.prototype.test = () => { throw Error('reentrant regex'); };
                    RegExp.prototype.exec = () => { throw Error('reentrant regex exec'); };
                    text(r); text([r,r]); store('raw',r);
                "#.into(),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(HashMap::new(), request, 60_000,
                events, Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
            let mut printed = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall {id, ..} => tx.send(RuntimeCommand::ToolResponse {
                        id, result:raw.clone(),
                    }).unwrap(),
                    RuntimeEvent::ContentItem {item:FunctionCallOutputContentItem::InputText {text}, ..} =>
                        printed.push(text),
                    RuntimeEvent::Result {error_text, output_loss, stored_value_writes} => {
                        assert_eq!(error_text, None);
                        assert_eq!(output_loss, None);
                        assert_eq!(*stored_value_writes["raw"].value, raw);
                        break;
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!(printed.len(), 2);
            if printed[0].contains("[source ") {
                assert!(printed[0].ends_with(output));
                assert!(printed[1].ends_with(output));
                let envelope: Value = serde_json::from_str(printed[0].split_once('\n').unwrap().0).unwrap();
                assert_eq!(envelope["output_lines"], output.split('\n').count());
            } else {
                assert_eq!(serde_json::from_str::<Value>(&printed[0]).unwrap()["output"], output);
                assert_eq!(serde_json::from_str::<Value>(&printed[1]).unwrap()[1]["output"], output);
            }
        }
    }

    #[tokio::test]
    async fn critical_path_shared_ranges_keep_unicode_boundaries_and_mutation_guards() {
        let body = "xλ😀\n".repeat(20_000);
        let size = body.len();
        let mut rows = vec![json!({"status":"ok","complete":true,"text":body,
            "canonical_range":{"start":0,"end":size}})];
        // Reverse order forces both extension and backwards checkpoint lookup.
        for index in (1..=80).rev() {
            rows.push(json!({"status":"ok","complete":true,"shared":true,
                "canonical_range":{"start":size-index*8,"end":size-(index-1)*8}}));
        }
        rows.push(json!({"status":"ok","complete":true,"shared":true,
            "canonical_range":{"start":size-6,"end":size}}));
        let raw = json!({"artifact_id":"snapshot","source_sha256":"hash","results":rows});
        let (events, mut rx) = mpsc::unbounded_channel();
        let request = ExecuteRequest {
            state_path:None, tool_call_id:"critical-path-projection".into(),
            enabled_tools:vec![ToolDefinition {
                name:"read_file".into(), tool_name:ToolName::plain("read_file"),
                kind:CodeModeToolKind::Function, description:"".into(),
                input_schema:None, output_schema:None, default_timeout_ms:None,
            }].into(),
            source:r#"
                const r = await tools.read_file({});
                text(r.results.slice(1,81).reverse());
                text(r.results[81]);
                text([r.results[1],r.results[1]]);
                r.results[0].text = 'mutated';
                text(r.results[1]);
            "#.into(),
            yield_time_ms:None, max_output_tokens:None, default_tool_timeout_ms:None,
        };
        let (tx, _termination) = spawn_runtime(HashMap::new(), request, 60_000, events,
            Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
        let mut printed = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
                RuntimeEvent::Started => {},
                RuntimeEvent::ToolCall {id, ..} => tx.send(RuntimeCommand::ToolResponse {id, result:raw.clone()}).unwrap(),
                RuntimeEvent::ContentItem {item:FunctionCallOutputContentItem::InputText {text}, ..} => printed.push(text),
                RuntimeEvent::Result {error_text, output_loss, ..} => {
                    assert_eq!(error_text, None); assert_eq!(output_loss, None); break;
                },
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(printed.len(), 4);
        assert_eq!(printed[0].matches("xλ😀\n").count(), 80);
        assert!(!printed[0].contains("\"shared\":true"));
        let invalid: Value = serde_json::from_str(&printed[1]).unwrap();
        assert_eq!(invalid["shared"], true);
        assert_eq!(invalid["recovery"]["selectors"][0]["start"], size-6);
        assert_eq!(printed[2].matches("xλ😀\n").count(), 2);
        let changed: Value = serde_json::from_str(&printed[3]).unwrap();
        assert_eq!(changed["shared"], true);
        assert!(changed.get("text").is_none());
        assert!(changed.get("recovery").is_none(), "mutation cannot inherit snapshot provenance");
    }

    #[tokio::test]
    async fn stored_presentation_avoids_an_overflow_recovery_round_trip() {
        async fn cell(source: &str, stored: HashMap<String, StoredValue>, raw: &Value, limit: usize)
            -> (Vec<String>, HashMap<String, StoredValue>, bool)
        {
            let (events, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None, tool_call_id: "stored-presentation".into(),
                enabled_tools: vec![ToolDefinition {
                    name: "read_file".into(), tool_name: ToolName::plain("read_file"),
                    kind: CodeModeToolKind::Function, description: String::new().into(),
                    input_schema: None, output_schema: None, default_timeout_ms: None,
                }].into(),
                source: source.into(), yield_time_ms: None, max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(stored, request, 60_000, events,
                Arc::new(OutputAdmission::new(limit)), None).await.unwrap();
            let mut printed = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx.send(RuntimeCommand::ToolResponse { id, result: raw.clone() }).unwrap(),
                    RuntimeEvent::ContentItem { item: FunctionCallOutputContentItem::InputText { text }, .. } => printed.push(text),
                    RuntimeEvent::Result { error_text, stored_value_writes, output_loss } => {
                        assert_eq!(error_text, None);
                        return (printed, stored_value_writes, output_loss.is_some());
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
        }
        let raw = json!({"path":"source.rs", "source_sha256":"sha", "canonical_sha256":"sha",
            "environment_id":"fixture-env", "canonical_uri":"file:///source.rs",
            "complete":true, "delivered_selection_complete":true, "artifact_id":"saved",
            "results":[{"status":"ok", "complete":true, "text":"\"\\\n".repeat(5_000)}]});
        let (before, saved, loss) = cell(
            "const r = await tools.read_file({}); store('raw', r); store('plain', JSON.parse(JSON.stringify(r))); store('batch', {items:[r]}); store('row', r.results[0]); text(r);",
            HashMap::new(), &raw, MAX_BUFFERED_OUTPUT_BYTES,
        ).await;
        assert!(!loss);
        assert_eq!(*saved["raw"].value, raw);
        assert!(saved["raw"].presentation.is_some());
        assert!(saved["plain"].presentation.is_none());
        assert!(saved["batch"].presentation.is_some());
        assert!(saved["row"].presentation.is_some());
        assert!(serde_json::to_vec(saved["raw"].presentation.as_ref().unwrap()).unwrap().len() < 1_024,
            "presentation must not duplicate retained source text");
        let limit = serde_json::to_vec(&FunctionCallOutputContentItem::InputText {
            text: before[0].clone(),
        }).unwrap().len() + 128;
        let (after, _, loss) = cell("text(load('raw'));", saved.clone(), &raw, limit).await;
        assert_eq!(after.len(), before.len());
        for (after, before) in after.iter().zip(&before) {
            let (after_metadata, after_source) = after.split_once('\n').unwrap();
            let (before_metadata, before_source) = before.split_once('\n').unwrap();
            assert_eq!(serde_json::from_str::<Value>(after_metadata).unwrap(),
                serde_json::from_str::<Value>(before_metadata).unwrap());
            assert_eq!(after_source, before_source, "retained source bytes stay exact");
        }
        assert!(!loss, "trusted load must fit without an overflow/recovery call");
        let (_, _, old_loss) = cell("text(load('plain'));", saved.clone(), &raw, limit).await;
        assert!(old_loss, "the former plain-JSON load requires overflow recovery at the same budget");
        let (batch, _, loss) = cell("text(load('batch')); text(load('row'));", saved.clone(), &raw, MAX_BUFFERED_OUTPUT_BYTES).await;
        assert!(!loss);
        assert!(batch.iter().all(|text| text.contains("[source ")));
        assert!(batch[1].contains("\"source_sha256\":\"sha\""));
        assert!(batch[1].contains("\"environment_id\":\"fixture-env\""));
        assert!(batch[1].contains("\"canonical_uri\":\"file:///source.rs\""));
        let (changed, _, loss) = cell("const r=load('raw'); r.results[0].text='changed'; text(r);", saved, &raw, MAX_BUFFERED_OUTPUT_BYTES).await;
        assert!(!loss);
        assert_eq!(serde_json::from_str::<Value>(&changed[0]).unwrap()["results"][0]["text"], "changed");
    }

    #[tokio::test]
    async fn mcp_text_framing_preserves_evidence_and_avoids_recovery_after_load() {
        async fn cell(source: &str, stored: HashMap<String, StoredValue>, raw: &Value, limit: usize)
            -> (Vec<String>, HashMap<String, StoredValue>, bool)
        {
            let (events, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None, tool_call_id: "mcp-presentation".into(),
                enabled_tools: vec![ToolDefinition {
                    name: "mcp__node_repl__js".into(), tool_name: ToolName::plain("mcp__node_repl__js"),
                    kind: CodeModeToolKind::Function, description: String::new().into(),
                    input_schema: None, output_schema: None, default_timeout_ms: None,
                }].into(),
                source: source.into(), yield_time_ms: None, max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(stored, request, 60_000, events,
                Arc::new(OutputAdmission::new(limit)), None).await.unwrap();
            let mut printed = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx.send(RuntimeCommand::ToolResponse { id, result: raw.clone() }).unwrap(),
                    RuntimeEvent::ContentItem { item: FunctionCallOutputContentItem::InputText { text }, .. } => printed.push(text),
                    RuntimeEvent::Result { error_text, stored_value_writes, output_loss } => {
                        assert_eq!(error_text, None);
                        return (printed, stored_value_writes, output_loss.is_some());
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
        }
        let source = "\"quoted\" \\ λ\r\n".repeat(2_000);
        let raw = json!({"content":[
            {"type":"text","text":source,"annotations":{"priority":1}},
            {"type":"image","data":"abc","mimeType":"image/png"},
            {"type":"text","text":"keep\ninline","text_lines":"caller owned"},
            {"type":"text","text":null}],"isError":true,"_meta":{"id":7}});
        let (before, saved, loss) = cell(
            "const r=await tools.mcp__node_repl__js({}); store('raw',r); store('plain',JSON.parse(JSON.stringify(r))); text(r);",
            HashMap::new(), &raw, MAX_BUFFERED_OUTPUT_BYTES,
        ).await;
        assert!(!loss);
        assert_eq!(*saved["raw"].value, raw);
        assert!(saved["raw"].presentation.is_some());
        assert!(saved["plain"].presentation.is_none());
        assert!(serde_json::to_vec(saved["raw"].presentation.as_ref().unwrap()).unwrap().len() < 1_024);
        let (metadata, body) = before[0].split_once('\n').unwrap();
        let mut envelope: Value = serde_json::from_str(metadata).unwrap();
        let (header, text) = body.split_once('\n').unwrap();
        assert_eq!(text, source, "Unicode, CRLF and escaped text must remain exact");
        assert!(header.starts_with("[source "));
        assert!(header.contains("content[0].text"));
        assert!(envelope["content"][0].as_object_mut().unwrap().remove("text_lines").is_some());
        envelope["content"][0]["text"] = json!(text);
        assert_eq!(envelope, raw, "all flags, annotations and non-text blocks survive");
        let limit = serde_json::to_vec(&FunctionCallOutputContentItem::InputText { text: before[0].clone() }).unwrap().len() + 128;
        let (after, _, loss) = cell("text(load('raw'));", saved.clone(), &raw, limit).await;
        assert!(!loss);
        assert_eq!(after, before);
        let (_, _, old_loss) = cell("text(load('plain'));", saved.clone(), &raw, limit).await;
        assert!(old_loss, "unframed JSON requires recovery at the same budget");
        let (changed, _, loss) = cell("const r=load('raw'); r.content[0].text='changed'; text(r);", saved, &raw, MAX_BUFFERED_OUTPUT_BYTES).await;
        assert!(!loss);
        let mut expected = raw;
        expected["content"][0]["text"] = json!("changed");
        assert_eq!(serde_json::from_str::<Value>(&changed[0]).unwrap(), expected);
        let short = json!({"content":[{"type":"text","text":"short\nmessage"}],"isError":false});
        let (printed, _, loss) = cell("text(await tools.mcp__node_repl__js({}));", HashMap::new(), &short, MAX_BUFFERED_OUTPUT_BYTES).await;
        assert!(!loss);
        assert_eq!(serde_json::from_str::<Value>(&printed[0]).unwrap(), short, "small results must not grow");
    }

    #[tokio::test]
    async fn verified10_explicit_formatter_survives_clone_annotation_and_storage() {
        let raw = json!({"output":"display λ\n", "stdout":"exact stdout ".repeat(600), "stderr":"",
            "streams_complete":true, "output_reduced":true, "output_complete":false,
            "execution_state":"exited", "exit_code":0});
        let (event_tx, mut rx) = mpsc::unbounded_channel();
        let request = ExecuteRequest {
            state_path: None, tool_call_id: "retained-formatter".into(),
            enabled_tools: vec![ToolDefinition {
                name:"exec_command".into(), tool_name:ToolName::plain("exec_command"),
                kind:CodeModeToolKind::Function, description:"".into(), input_schema:None,
                output_schema:None, default_timeout_ms:None,
            }].into(),
            source:r#"
                const r = await tools.exec_command({});
                store('raw', r);
                const copy = JSON.parse(JSON.stringify(r));
                text(r);
                text(format_tool_result('exec_command', copy));
                text(format_tool_result('exec_command', load('raw')));
                text(format_tool_result('exec_command', {...copy, note:'annotation'}));
                if (copy.stdout !== r.stdout || load('raw').stdout !== r.stdout) throw Error('raw changed');
                text(copy);
            "#.into(),
            yield_time_ms:None, max_output_tokens:None, default_tool_timeout_ms:None,
        };
        let (tx, _termination) = spawn_runtime(HashMap::new(), request, 60_000, event_tx,
            Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
        let mut printed = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
                RuntimeEvent::Started => {},
                RuntimeEvent::ToolCall { id, .. } => tx.send(RuntimeCommand::ToolResponse { id, result:raw.clone() }).unwrap(),
                RuntimeEvent::ContentItem { item:FunctionCallOutputContentItem::InputText { text }, .. } => printed.push(text),
                RuntimeEvent::Result { error_text, stored_value_writes, output_loss } => {
                    assert_eq!(error_text, None);
                    assert_eq!(output_loss, None);
                    assert_eq!(*stored_value_writes["raw"].value, raw);
                    break;
                },
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(printed.len(), 5);
        assert_eq!(printed[0], printed[1]);
        assert_eq!(printed[0], printed[2]);
        assert!(printed[3].contains("annotation"));
        assert!(!printed[..4].iter().any(|text| text.contains("exact stdout")));
        assert!(printed[4].contains("exact stdout"), "unregistered arbitrary JSON remains exact");
    }

    #[tokio::test]
    async fn projection_preserves_raw_values_and_only_compacts_registered_objects() {
        for (name, raw) in [
            (
                "read_file",
                json!({"source_sha256":"rev", "canonical_sha256":"rev", "complete":true,
                "delivered_selection_complete":true,"artifact_id":null,"results":[{"text":"λ 日本語"}]}),
            ),
            (
                "mcp__test__mirror",
                json!({"structuredContent":{"x":1},"isError":true,"_meta":{"id":7},
                "content":[{"type":"text","text":"{\"x\":1}"},{"type":"text","text":"caption"}]}),
            ),
            (
                "apply_patch",
                json!({"success":true,"changes_exact":true,
                    "text":"Success. Updated the following files:\nM λ\n",
                    "changes":[{"kind":"update","path":"λ","move_path":null}]}),
            ),
            (
                "read_file",
                json!({"artifact_id":"search-snapshot", "source_sha256":"revision", "complete":false,
                    "results":[{"status":"ok", "selector":{"kind":"search","query":"λ","start_byte":0},
                        "continuation":{"kind":"search","query":"λ","start_byte":2},
                        "child_selectors":[{"kind":"lines","start":1,"end":1}],
                        "value":{"query":"λ","start_byte":0,"matches_returned":1,
                            "total_matches":2,"remaining_match_count":1,"coverage_complete":true,
                            "matches":[{"line":1,"end_line":1,"start_byte":0,"end_byte":2}],
                            "hydrated_ranges":[{"selector":{"kind":"lines","start":1,"end":1},
                                "canonical_range":{"start":0,"end":2},"exact_bytes":2,"text":"λ"}]}}]}),
            ),
        ] {
            let projected =
                codex_code_mode_protocol::model_visible_tool_result(&ToolName::plain(name), &raw)
                    .unwrap();
            let (event_tx, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None,
                tool_call_id: "projection".into(),
                enabled_tools: vec![ToolDefinition {
                    name: name.into(),
                    tool_name: ToolName::plain(name),
                    kind: CodeModeToolKind::Function,
                    description: "".into(),
                    input_schema: None,
                    output_schema: None,
                    default_timeout_ms: None,
                }].into(),
                source: format!(
                    r#"
                    const r = await tools.{name}({{}});
                    store('raw', r);
                    text(r); text({{wrapped:[r]}}); console.log(r);
                    text(JSON.parse(JSON.stringify(r)));
                    const child = r.results ? r.results[0] : r.changes ? r.changes[0] : r.structuredContent;
                    Object.defineProperty(child, 'toJSON', {{value: () => 'CUSTOM', configurable:true}});
                    text(r);
                    delete child.toJSON;
                    r.changed = true; text(r);
                    let reads = 0;
                    Object.defineProperty(r, 'accessor', {{enumerable:true, get() {{ reads++; return 7; }} }});
                    text(r); text(reads);
                "#
                ),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(
                HashMap::new(),
                request,
                60_000,
                event_tx,
                Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)),
                None,
            )
            .await
            .unwrap();
            let mut printed = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: raw.clone(),
                        })
                        .unwrap(),
                    RuntimeEvent::ContentItem {
                        item: FunctionCallOutputContentItem::InputText { text },
                        ..
                    } => {
                        printed.push(serde_json::from_str::<Value>(&text).unwrap());
                    }
                    RuntimeEvent::Result {
                        error_text,
                        stored_value_writes,
                        output_loss,
                    } => {
                        assert_eq!(error_text, None);
                        assert_eq!(output_loss, None);
                        assert_eq!(*stored_value_writes["raw"].value, raw);
                        break;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            let mut transformed = raw.clone();
            if transformed.get("results").is_some() {
                transformed["results"][0] = json!("CUSTOM");
            } else if transformed.get("changes").is_some() {
                transformed["changes"][0] = json!("CUSTOM");
            } else {
                transformed["structuredContent"] = json!("CUSTOM");
            }
            let mut changed = raw.clone();
            changed["changed"] = json!(true);
            let mut accessor = changed.clone();
            accessor["accessor"] = json!(7);
            assert_eq!(
                printed,
                vec![
                    projected.clone(),
                    json!({"wrapped":[projected.clone()]}),
                    projected,
                    raw,
                    transformed,
                    changed,
                    accessor,
                    json!(1)
                ]
            );
        }
    }

    #[tokio::test]
    async fn whole_and_batched_text_results_print_raw_after_counted_envelopes() {
        let source = "fn a() {\r\n    \"λ\\path\";\r\n}\r\n".repeat(128);
        let lines = source.split('\n').count();
        let command = json!({"chunk_id":"transport", "output":source,
            "exit_code":0, "execution_state":"exited", "process_exited":true,
            "output_complete":true});
        let mut command_envelope = codex_code_mode_protocol::model_visible_tool_result(
            &ToolName::plain("exec_command"),
            &command,
        )
        .unwrap();
        command_envelope.as_object_mut().unwrap().remove("output");
        command_envelope["output_lines"] = json!(lines);
        // Recovered source keeps its CRLF bytes and quotes unescaped too.
        let recovery = json!({"artifact_id":"retained", "complete":true,
            "results":[{"status":"ok", "text":"fn a() {\r\n    \"x\"\r\n}"}]});
        let recovery_envelope = json!({"artifact_id":"retained", "complete":true,
            "results":[{"status":"ok", "text_lines":3}]});
        let search = json!({"artifact_id":"search-evidence", "complete":false,
            "results":[{"selector":{"kind":"search","query":"fn a"}, "status":"ok",
                "continuation":{"kind":"search","query":"fn a","start_byte":source.len()},
                "value":{"coverage_complete":false, "remaining_match_count":1,
                    "hydrated_ranges":[{"canonical_range":{"start":0,"end":source.len()}, "text":source},
                        {"canonical_range":{"start":0,"end":source.len()},"shared":true},
                        {"canonical_range":{"start":source.len(),"end":source.len()+1},"data_base64":"/w=="}]}}]});
        let mut search_envelope = search.clone();
        let hydrated = &mut search_envelope["results"][0]["value"]["hydrated_ranges"][0];
        hydrated.as_object_mut().unwrap().remove("text");
        hydrated["text_lines"] = json!(lines);
        for (fixture, name, raw, envelope, body) in [
            (
                "command",
                "exec_command",
                command,
                command_envelope,
                source.as_str(),
            ),
            (
                "recovery",
                "read_tool_output",
                recovery,
                recovery_envelope,
                "fn a() {\r\n    \"x\"\r\n}",
            ),
            ("search", "read_tool_output", search, search_envelope, source.as_str()),
        ] {
            let (event_tx, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None,
                tool_call_id: "raw-projection".into(),
                enabled_tools: vec![ToolDefinition {
                    name: name.into(),
                    tool_name: ToolName::plain(name),
                    kind: CodeModeToolKind::Function,
                    description: "".into(),
                    input_schema: None,
                    output_schema: None,
                    default_timeout_ms: None,
                }]
                .into(),
                source: format!(
                    "const r = await tools.{name}({{}}); text(r); text({{part:7, results:[r,r]}}); store('raw', r); if(r.results){{text(r.results); text(r.results[0]); if(r.results[0].value?.hydrated_ranges) text(r.results[0].value.hydrated_ranges); text(JSON.parse(JSON.stringify(r.results)));}} r.extra = 1; text(r); if(r.results) text(r.results);"
                ),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            };
            let (tx, _termination) = spawn_runtime(
                HashMap::new(),
                request,
                60_000,
                event_tx,
                Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)),
                None,
            )
            .await
            .unwrap();
            let mut printed = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    RuntimeEvent::Started => {}
                    RuntimeEvent::ToolCall { id, .. } => tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: raw.clone(),
                        })
                        .unwrap(),
                    RuntimeEvent::ContentItem {
                        item: FunctionCallOutputContentItem::InputText { text },
                        ..
                    } => printed.push(text),
                    RuntimeEvent::Result { error_text, stored_value_writes, output_loss } => {
                        assert_eq!(error_text, None);
                        assert_eq!(output_loss, None);
                        assert_eq!(*stored_value_writes["raw"].value, raw);
                        break;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            assert_eq!(printed.len(), if fixture == "command" { 3 } else if fixture == "search" { 8 } else { 7 }, "{name}");
            let (header, printed_body) = printed[0].split_once('\n').unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(header).unwrap(),
                envelope,
                "{name}"
            );
            let (label, printed_body) = printed_body.split_once('\n').unwrap();
            assert!(label.starts_with("[source {"));
            assert_eq!(printed_body, body, "{name}");
            let (header, printed_body) = printed[1].split_once('\n').unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(header).unwrap(),
                json!({"part":7, "results":[envelope.clone(), envelope.clone()]}),
                "batch identity and result ordering must survive projection"
            );
            let (first_label, first_body) = printed_body.split_once('\n').unwrap();
            assert!(first_label.contains("\"body\":1"));
            let rest = first_body.strip_prefix(body).unwrap().strip_prefix('\n').unwrap();
            let (second_label, second_body) = rest.split_once('\n').unwrap();
            assert!(second_label.contains("\"body\":2"));
            if body.len() >= 512 {
                assert!(second_label.contains("\"same_as_body\":1"));
                assert!(second_body.is_empty());
                assert_eq!(first_body.strip_suffix(rest).unwrap().trim_end_matches('\n'), body.trim_end_matches('\n'));
            } else {
                assert_eq!(second_body, body, "{name}");
            }
            let projected = codex_code_mode_protocol::model_visible_tool_result(&ToolName::plain(name), &raw).unwrap();
            let old_batch = json!({"part":7,"results":[projected.clone(),projected]}).to_string();
            if fixture != "recovery" {
                assert!(printed[1].len() < old_batch.len());
            }
            println!("projection_audit {fixture} raw_bytes={} whole_bytes={} batch_before_bytes={} batch_after_bytes={}",
                raw.to_string().len(), printed[0].len(), old_batch.len(), printed[1].len());
            if fixture != "command" {
                let mut selected = vec![envelope["results"].clone(), envelope["results"][0].clone()];
                if fixture == "search" {
                    selected.push(envelope["results"][0]["value"]["hydrated_ranges"].clone());
                }
                for (index, expected) in selected.iter().enumerate() {
                    let (header, text) = printed[index + 2].split_once('\n').unwrap();
                    assert_eq!(serde_json::from_str::<Value>(header).unwrap(), *expected);
                    let (label, text) = text.split_once('\n').unwrap();
                    assert!(label.contains("artifact_id"));
                    assert_eq!(text, body, "selected fields must preserve exact text and coverage");
                }
                assert_eq!(serde_json::from_str::<Value>(&printed[2 + selected.len()]).unwrap(), raw["results"]);
                assert_eq!(serde_json::from_str::<Value>(printed.last().unwrap()).unwrap(), raw["results"],
                    "a changed parent must disable child projection too");
                if fixture == "search" {
                    assert!(printed[2].len() < raw["results"].to_string().len());
                }
                println!("projection_audit selected_{fixture} before_bytes={} after_bytes={}",
                    raw["results"].to_string().len(), printed[2].len());
            }
            // Added inert command metadata preserves the unchanged projection;
            // changed source parents still disable their fragment projections.
            let mut changed = raw;
            changed["extra"] = json!(1);
            if fixture == "command" {
                let (header, text) = printed.last().unwrap().split_once('\n').unwrap();
                let mut expected = envelope.clone();
                expected["extra"] = json!(1);
                assert_eq!(serde_json::from_str::<Value>(header).unwrap(), expected);
                assert_eq!(text.split_once('\n').unwrap().1, body);
            } else { assert_eq!(
                serde_json::from_str::<Value>(&printed[printed.len() - if fixture == "command" { 1 } else { 2 }]).unwrap(),
                changed,
                "{name}"
            ); }
        }
    }

    #[tokio::test]
    async fn verified10_selected_shared_evidence_and_safe_annotations() {
        let source = "λ\r\nunique evidence\n".repeat(32);
        let search = json!({"artifact_id":"snapshot", "source_sha256":"revision", "results":[
            {"selector":{"kind":"search"},"status":"ok","value":{"hydrated_ranges":[
                {"canonical_range":{"start":0,"end":source.len()},"text":source}]}},
            {"selector":{"kind":"search"},"status":"ok","value":{"hydrated_ranges":[
                {"canonical_range":{"start":0,"end":source.len()},"shared":true}]}}
        ]});
        for (name, raw, source) in [
            ("read_tool_output", search, r#"
                const r = await tools.read_tool_output({});
                text(r); text(r.results[1]); text(r.results[1].value.hydrated_ranges[0]);
                const bytes = r.results[0].value.hydrated_ranges[0].text;
                if (r.results[1].value.hydrated_ranges[0].text !== undefined) throw Error('native mutated');
                store('expected', bytes);
            "#),
            ("exec_command", json!({"output":"compact output", "stdout":"raw stdout ".repeat(1000),
                "stderr":"raw stderr ".repeat(1000), "streams_complete":true, "exit_code":0,
                "process_exited":true}), r#"
                const r = await tools.exec_command({});
                text(r); r.reviewed = true; text(r);
                r.stdout = 'CHANGED'; text(r); r.stdout = 'raw stdout '.repeat(1000);
                let reads = 0;
                Object.defineProperty(r, 'getter', {enumerable:true, configurable:true, get(){reads++; return 7;}});
                text(r); if(reads !== 1) throw Error('getter invoked more than once'); delete r.getter;
                r.extra = new Proxy({ok:true}, {}); text(r); delete r.extra;
                r.extra = {toJSON(){return 'CUSTOM';}}; text(r); delete r.extra;
                r.toJSON = () => 'CUSTOM ROOT'; text(r);
            "#),
        ] {
            let (event_tx, mut rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {
                state_path: None, tool_call_id: "verified10".into(),
                enabled_tools: vec![ToolDefinition {
                    name:name.into(), tool_name:ToolName::plain(name), kind:CodeModeToolKind::Function,
                    description:"".into(), input_schema:None, output_schema:None, default_timeout_ms:None,
                }].into(), source:source.into(), yield_time_ms:None, max_output_tokens:None, default_tool_timeout_ms:None,
            };
            let (tx, _termination) = spawn_runtime(HashMap::new(), request, 60_000, event_tx,
                Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
            let mut printed = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
                    RuntimeEvent::Started => {},
                    RuntimeEvent::ToolCall {id, ..} => tx.send(RuntimeCommand::ToolResponse {id, result:raw.clone()}).unwrap(),
                    RuntimeEvent::ContentItem {item:FunctionCallOutputContentItem::InputText {text}, ..} => printed.push(text),
                    RuntimeEvent::Result {error_text, output_loss, ..} => {
                        assert_eq!(error_text, None); assert_eq!(output_loss, None); break;
                    },
                    other => panic!("unexpected {other:?}"),
                }
            }
            if name == "read_tool_output" {
                let expected = raw["results"][0]["value"]["hydrated_ranges"][0]["text"].as_str().unwrap();
                assert_eq!(printed.len(), 3);
                for rendered in printed {
                    let (_, body) = rendered.split_once('\n').unwrap();
                    let (identity, bytes) = body.split_once('\n').unwrap();
                    assert!(identity.contains("snapshot"));
                    assert_eq!(bytes, expected);
                }
            } else {
                assert_eq!(printed.len(), 7);
                assert!(printed[1].len() < printed[0].len() + 32);
                assert!(printed[1].contains("\"reviewed\":true"));
                for text in &printed[2..6] {
                    let value: Value = serde_json::from_str(text).unwrap();
                    assert!(value["stderr"].as_str().unwrap().starts_with("raw stderr"));
                }
                assert_eq!(serde_json::from_str::<Value>(&printed[2]).unwrap()["stdout"], "CHANGED");
                assert_eq!(serde_json::from_str::<Value>(&printed[5]).unwrap()["extra"], "CUSTOM");
                assert_eq!(printed[6], "\"CUSTOM ROOT\"");
            }
        }
    }
}
