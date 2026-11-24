{
const monotonic_now = __codex_monotonic_now;

function schema_fixture(root, {string = ""} = {}) {
    if (typeof string !== "string" || string.length > 4096)
        throw new TypeError("fixture string must be at most 4096 characters");
    const record = value => value !== null && typeof value === "object" && !Array.isArray(value);
    const annotations = new Set(["$schema","definitions","$defs","description","title","$comment","default"]);
    const primitives = {string, integer:0, number:0, boolean:false, null:null};
    let remaining = 1024, bytes = 65536;
    function charge(value) {
        bytes -= new TextEncoder().encode(JSON.stringify(value)).length;
        if (bytes < 0) throw new RangeError("fixture output budget exceeded");
        return value;
    }
    function visit(node, depth, stack) {
        if (!record(node) || depth > 64 || --remaining < 0)
            throw new TypeError("expected a bounded schema object");
        const keys = Object.keys(node).filter(k => !annotations.has(k));
        if (keys.length === 1 && keys[0] === "$ref") {
            const ref = node.$ref;
            if (typeof ref !== "string" || ref.length > 4096 || !ref.startsWith("#/") ||
                /%|~(?:[^01]|$)/.test(ref) || stack.has(ref))
                throw new TypeError("expected an acyclic local JSON Pointer reference");
            let target = root;
            for (const part of ref.slice(2).split("/")) {
                const key = part.replace(/~1/g, "/").replace(/~0/g, "~");
                if (!record(target) || !Object.hasOwn(target,key))
                    throw new TypeError("missing local schema reference");
                target = target[key];
            }
            stack.add(ref);
            try { return visit(target, depth + 1, stack); }
            finally { stack.delete(ref); }
        }
        if (keys.length === 1 && keys[0] === "const")
            return JSON.parse(JSON.stringify(charge(node.const)));
        if (node.type === "object" && keys.every(k => ["type","properties","required","additionalProperties"].includes(k))) {
            const {properties = {}, required = [], additionalProperties = true} = node;
            if (!record(properties) || typeof additionalProperties !== "boolean" ||
                !Array.isArray(required) || required.length > remaining ||
                new Set(required).size !== required.length ||
                required.some(k => typeof k !== "string" || !Object.hasOwn(properties,k)))
                throw new TypeError("invalid required fixture fields");
            return Object.fromEntries(required.map(k => [charge(k), visit(properties[k], depth + 1, stack)]));
        }
        if (node.type === "array" && keys.every(k => ["type","items","minItems"].includes(k))) {
            const count = Object.hasOwn(node, "minItems") ? node.minItems : 0;
            if (!Number.isInteger(count) || count < 0 || count > 32 || !record(node.items))
                throw new TypeError("expected array item schema and minItems in 0..32");
            return Array.from({length:count}, () => visit(node.items, depth+1, stack));
        }
        if (keys.length === 1 && keys[0] === "type" && Object.hasOwn(primitives, node.type) && typeof node.type === "string")
            return charge(primitives[node.type]);
        throw new TypeError("unsupported fixture keywords: " + keys.join(", "));
    }
    return visit(root, 0, new Set());
}

async function minimize_diagnostic(groups, probe, {max_runs = 64, seconds = 30} = {}) {
    if (!Array.isArray(groups) || groups.length > 256 || typeof probe !== "function" ||
        !Number.isInteger(max_runs) || max_runs < 1 || max_runs > 512 ||
        !Number.isFinite(seconds) || seconds <= 0 || seconds > 300)
        throw new TypeError("expected bounded groups, probe and limits");
    if (Object.keys(groups).length !== groups.length || Array.from(groups).some((_, i) => !Object.hasOwn(groups,i)))
        throw new TypeError("expected dense argument groups");
    const values = groups.slice();
    let ids = values.map((_, i) => i), n = 2, matched = false;
    const runs = [], seen = new Map(), end = monotonic_now() + seconds * 1000;
    const probeOnce = async (indices) => {
        const key = indices.join(",");
        if (seen.has(key)) return seen.get(key);
        const remaining = end - monotonic_now();
        if (remaining <= 0 || runs.length >= max_runs) return null;
        const result = await probe(indices.map(i => values[i]), remaining);
        if (result !== true && result !== false && result !== null)
            throw new TypeError("probe must return true, false or null");
        runs.push({indices, match:result});
        seen.set(key, result);
        return monotonic_now() < end ? result : null;
    };
    const outcome = status => ({status, groups:matched ? ids.map(i => values[i]) : null, runs});
    const first = await probeOnce(ids);
    if (first === null) return outcome("incomplete");
    if (!first) return outcome("not_reproduced");
    matched = true;
    while (ids.length) {
        const width = Math.ceil(ids.length / n);
        let found = false;
        for (let i = 0; i < ids.length; i += width) {
            const rest = ids.slice(0,i).concat(ids.slice(i+width));
            const match = await probeOnce(rest);
            if (match === null) return outcome("incomplete");
            if (match) {
                ids = rest;
                n = Math.max(2, n-1);
                found = true;
                break;
            }
        }
        if (!found) {
            if (n >= ids.length) break;
            n = Math.min(ids.length, 2*n);
        }
    }
    return outcome("one_minimal");
}

Object.assign(globalThis, {schema_fixture, minimize_diagnostic});
}
