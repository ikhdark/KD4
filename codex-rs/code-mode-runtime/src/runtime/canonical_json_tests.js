// Shared by the native store path and the isolated codec regression probe.
{
  const check = (condition, message) => { if (!condition) throw Error(message); };
  let value = "x", tailVisits = 0, rejected = false;
  // Only twenty distinct objects, but JSON's repeated occurrences exceed the
  // 8 MiB transport/storage ceiling. Stop before visiting subsequent properties.
  for (let i = 0; i < 20; ++i) value = {a: value, b: value};
  try {
    store("oversized", {body: value, get tail() { ++tailVisits; return true; }});
  } catch (error) { rejected = true; }
  check(rejected && tailVisits === 0 && load("oversized") === undefined,
    "serialization materialized the oversized graph or mutated storage");

  let conversions = 0, numberConversions = 0, stringConversions = 0;
  const number = new Number(7);
  number.valueOf = () => { ++numberConversions; return 9; };
  const string = new String("original");
  string[Symbol.toPrimitive] = hint => {
    check(hint === "string", "boxed string used the wrong coercion hint");
    ++stringConversions; return "converted";
  };
  const positive = {
    text: "λ😀\n\\\"\ud800\udc00",
    number, string, boolean: new Boolean(false),
    omitted: undefined,
    array: [undefined, , NaN, Infinity, -0, false],
    custom: {toJSON() { ++conversions; return {answer: 42}; }},
    exact: 9007199254740993n,
  };
  const reparented = new Number(1);
  Object.setPrototypeOf(reparented, Object.prototype);
  reparented.valueOf = () => 1;
  positive.reparented = reparented;
  store("valid", positive);
  const restored = load("valid");
  check(restored.text === positive.text && restored.number === 9 &&
    restored.string === "converted" && restored.boolean === false &&
    !Object.hasOwn(restored, "omitted") && restored.custom.answer === 42 &&
    restored.exact === 9007199254740993n && restored.reparented === 1 &&
    JSON.stringify(restored.array) === "[null,null,null,null,0,false]" &&
    conversions === 1 && numberConversions === 1 && stringConversions === 1,
    "bounded serialization changed valid JSON or repeated user code");
  deleteStored("valid");
  // The persisted serde_json representation contains Unicode scalar strings,
  // not lone UTF-16 surrogates. Invalid strings must fail atomically, while the
  // paired-surrogate case above must round-trip without loss.
  for (const text of ["\ud800", "\udc00", "prefix\ud800tail"]) {
    let invalidRejected = false;
    try { store("invalid-surrogate", {text}); }
    catch { invalidRejected = true; }
    check(invalidRejected && load("invalid-surrogate") === undefined,
      "an invalid Unicode string was silently replaced or persisted");
  }
}
text("bounded canonical JSON scenarios passed");
