// Pass the pristine realm's iterator and species identities to the native
// observer before any cell code runs. Native forwarding does not replace the
// argument's iterator, add a reaction, or consume a member itself.
(() => {
  const observe = globalThis.__codex_aggregate_observation;
  delete globalThis.__codex_aggregate_observation;
  if (typeof observe !== "function") return;
  observe(false, Promise, [
    Array.prototype,
    Object.getPrototypeOf([][Symbol.iterator]()),
    Symbol.species,
  ]);
})();
