// Pure comparisons over acquired JSON. Do not add I/O or bypass native tools.
function compareEngineeringValues(left, right, options = {}) {
    if (options.names !== undefined && (!Array.isArray(options.names) || options.names.length > 4096))
        throw new TypeError("expected a bounded array of selected variable names");
    const allowed = options.names === undefined ? null : new Set(options.names);
    const fold = (key) => options.windows && /^[\x00-\x7f]*$/.test(key) ? key.toUpperCase() : key;
    const normalized = allowed && new Set([...allowed].map(fold));
    const isRecord = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
    const absent = Symbol();
    const own = (value, key) => Object.hasOwn(value, key) ? value[key] : absent;
    const part = (key) => key.replace(/~/g, "~0").replace(/\//g, "~1");
    let visits = 0;
    const checkNumber = (v) => {
        if (typeof v === "number" && (!Number.isFinite(v) || (Number.isInteger(v) && !Number.isSafeInteger(v))))
            throw new TypeError("use JSON values without imprecise integers");
    };
    function visit(depth) {
        if (++visits > 60000 || depth > 64) throw new RangeError("engineering diff exceeds the visit budget");
    }
    function equal(a, b, depth) {
        visit(depth);
        checkNumber(a);
        checkNumber(b);
        if (a === b) return true;
        if (Array.isArray(a) && Array.isArray(b)) {
            return a.length === b.length && a.every((entry, i) => equal(entry, b[i], depth + 1));
        }
        return isRecord(a) && isRecord(b) && Object.keys(a).length === Object.keys(b).length
            && Object.keys(a).every((key) => equal(a[key], own(b, key), depth + 1));
    }
    const op = (a, b) => a === absent ? "added" : b === absent ? "removed" : "changed";
    if (normalized) {
        if (!Array.isArray(options.names) || !options.names.length || !isRecord(left) || !isRecord(right)
            || options.names.some((k) => typeof k !== "string" || !k || /[=\0]/.test(k)
                || (options.windows && !k.match(/^[\x00-\x7f]*$/)))) throw new TypeError("expected selected variable names");
        const project = (object) => {
            const result = new Map();
            for (const key of Object.keys(object)) {
                visit(0);
                const normalizedKey = fold(key);
                if (!normalized.has(normalizedKey)) continue;
                if (result.has(normalizedKey) || typeof object[key] !== "string")
                    throw new TypeError("selected variable is ambiguous or not a string");
                result.set(normalizedKey, object[key]);
            }
            return result;
        };
        const a = project(left), b = project(right);
        const changes = [], absent_from_both = [];
        for (const name of [...normalized].sort()) {
            const x = a.has(name) ? a.get(name) : absent, y = b.has(name) ? b.get(name) : absent;
            if (x === absent && y === absent) absent_from_both.push(name);
            if (x !== y) changes.push({name, operation:op(x, y)});
        }
        return {changes, absent_from_both};
    }
    const changes = [];
    function walk(a, b, path, depth) {
        visit(depth);
        if (isRecord(a) && isRecord(b)) {
            for (const key of [...new Set([...Object.keys(a), ...Object.keys(b)])].sort())
                walk(own(a, key), own(b, key), path + "/" + part(key), depth + 1);
        } else if (!equal(a, b, depth)) {
            changes.push({path, operation:op(a,b), ...(a === absent ? {} : {before:a}),
                ...(b === absent ? {} : {after:b})});
            if (changes.length > 4096) throw new RangeError("engineering diff exceeds 4096 changes");
        }
    }
    walk(left, right, "", 0);
    return changes;
}

Object.defineProperty(globalThis, "engineering_diff", {value: compareEngineeringValues});
