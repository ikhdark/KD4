// Run by the owning V8 runtime regression test.
{
  const check = (condition, message) => { if (!condition) throw Error(message); };
  const raised = (action, type) => {
    try { action(); } catch (error) { return error instanceof type; }
    return false;
  };
  const list = (view) => Array.from(view).join();
  const encoder = new TextEncoder();
  check(encoder.encoding === "utf-8" && list(encoder.encode()) === "", "encoder defaults");
  check(encoder.encode("a") instanceof Uint8Array, "encode result type");
  check(list(encoder.encode("aλ€😀")) === "97,206,187,226,130,172,240,159,152,128",
    "UTF-8 widths");
  // USVString conversion: every lone surrogate becomes U+FFFD.
  check(list(encoder.encode("\ud800x\udc00")) === "239,191,189,120,239,191,189",
    "lone surrogates");
  const whole = new Uint8Array(5);
  const fits = encoder.encodeInto("a😀b", whole);
  check(fits.read === 3 && fits.written === 5 && list(whole) === "97,240,159,152,128",
    "encodeInto counts UTF-16 units and bytes");
  const short = new Uint8Array(4);
  const split = encoder.encodeInto("ab😀", short);
  check(split.read === 2 && split.written === 2 && list(short) === "97,98,0,0",
    "encodeInto wrote part of a character");
  check(raised(() => encoder.encodeInto("a", [0]), TypeError), "encodeInto accepted an array");

  const decoder = new TextDecoder();
  check(decoder.encoding === "utf-8" && !decoder.fatal && !decoder.ignoreBOM,
    "decoder defaults");
  check(decoder.decode(encoder.encode("aλ€😀\ud800")) === "aλ€😀�", "round trip");
  check(decoder.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x61])) === "a", "BOM not stripped");
  check(new TextDecoder("utf-8", { ignoreBOM: true })
    .decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x61])) === "﻿a", "ignoreBOM dropped the BOM");
  // One replacement per maximal subpart; the offending byte is reprocessed.
  check(decoder.decode(new Uint8Array([0xf0, 0x9f, 0x98, 0x41])) === "�A",
    "truncated sequence");
  check(decoder.decode(new Uint8Array([0xc0, 0x80, 0xed, 0xa0, 0x80, 0xff])) ===
    "�".repeat(6), "overlong, surrogate and invalid bytes");
  check(decoder.decode(new Uint8Array([0xe2, 0x82])) === "�", "incomplete tail");
  // Streaming keeps a split BOM and character between chunks.
  let streamed = "";
  for (const chunk of [[0xef, 0xbb], [0xbf, 0xf0, 0x9f], [0x98, 0x80]])
    streamed += decoder.decode(new Uint8Array(chunk), { stream: true });
  streamed += decoder.decode();
  check(streamed === "😀", "streamed decode");
  check(decoder.decode(new Uint8Array([0x61]).buffer) === "a", "ArrayBuffer input");
  check(decoder.decode(new DataView(new Uint8Array([0x62, 0x63]).buffer, 1)) === "c",
    "view offset");
  check(raised(() => decoder.decode("text"), TypeError), "string input accepted");
  check(raised(() => new TextDecoder("utf-8", { fatal: true })
    .decode(new Uint8Array([0xff])), TypeError), "fatal decode replaced an error");
  check(raised(() => new TextDecoder("latin1"), RangeError), "unsupported label accepted");
  check(new TextDecoder(" UTF8 ").encoding === "utf-8", "label normalization");
  check(Object.prototype.toString.call(encoder) === "[object TextEncoder]" &&
    Object.prototype.toString.call(decoder) === "[object TextDecoder]", "interface tags");
}
text("text encoding scenarios passed");
