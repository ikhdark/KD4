// Async provenance for Promise aggregates. Each wrapper calls the original
// builtin with the same receiver and the same elements, in the same order; it
// adds no reactions, awaits, ticks or rejection handling. Only arrays passed
// to the intrinsic Promise are reported, after the builtin returned; every
// other receiver or iterable shape stays unknown to the causal capture.
(() => {
  const observe = globalThis.__codex_aggregate_observation;
  delete globalThis.__codex_aggregate_observation;
  if (typeof observe !== "function") return;
  const NativePromise = Promise;
  const apply = Reflect.apply;
  const defineProperty = Object.defineProperty;
  const isArray = Array.isArray;
  for (const [name, joinAll] of [["all", true], ["allSettled", true], ["race", false], ["any", false]]) {
    const original = NativePromise[name];
    if (typeof original !== "function") continue;
    const wrapped = {
      [name](iterable) {
        if (this !== NativePromise || !isArray(iterable)) return apply(original, this, [iterable]);
        const members = [];
        let count = 0;
        // The builtin drives this iterator exactly as it would drive the array.
        const tap = (function* () {
          for (const item of iterable) {
            members[count++] = item;
            yield item;
          }
        })();
        const result = apply(original, this, [tap]);
        observe(joinAll, result, members);
        return result;
      },
    }[name];
    defineProperty(NativePromise, name, { value: wrapped, writable: true, enumerable: false, configurable: true });
  }
})();
