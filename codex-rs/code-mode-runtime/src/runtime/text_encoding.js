// UTF-8 subset of the WHATWG Encoding API. V8 itself installs no TextEncoder or
// TextDecoder, yet cells reach for them to count or produce exact UTF-8 bytes.
// Lone surrogates encode as U+FFFD; decoding follows the standard's
// maximal-subpart replacement, BOM, fatal and streaming rules.
(() => {
  const REPLACEMENT = 0xfffd;
  // The scalar value at `index`, with a lone surrogate read as U+FFFD
  // (USVString conversion). Callers advance two units past astral values.
  const scalarAt = (string, index) => {
    const unit = string.charCodeAt(index);
    if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < string.length) {
      const next = string.charCodeAt(index + 1);
      if (next >= 0xdc00 && next <= 0xdfff)
        return ((unit - 0xd800) << 10) + (next - 0xdc00) + 0x10000;
    }
    return unit >= 0xd800 && unit <= 0xdfff ? REPLACEMENT : unit;
  };
  const width = (cp) =>
    cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
  const write = (bytes, offset, cp) => {
    if (cp < 0x80) {
      bytes[offset] = cp;
    } else if (cp < 0x800) {
      bytes[offset] = 0xc0 | (cp >> 6);
      bytes[offset + 1] = 0x80 | (cp & 0x3f);
    } else if (cp < 0x10000) {
      bytes[offset] = 0xe0 | (cp >> 12);
      bytes[offset + 1] = 0x80 | ((cp >> 6) & 0x3f);
      bytes[offset + 2] = 0x80 | (cp & 0x3f);
    } else {
      bytes[offset] = 0xf0 | (cp >> 18);
      bytes[offset + 1] = 0x80 | ((cp >> 12) & 0x3f);
      bytes[offset + 2] = 0x80 | ((cp >> 6) & 0x3f);
      bytes[offset + 3] = 0x80 | (cp & 0x3f);
    }
  };
  // WebIDL dictionaries accept undefined/null as empty; other primitives fail.
  const dictionary = (value, name) => {
    if (value === undefined || value === null) return {};
    if (typeof value !== "object" && typeof value !== "function")
      throw new TypeError(`${name} must be an object`);
    return value;
  };

  class TextEncoder {
    get encoding() {
      return "utf-8";
    }

    encode(input = "") {
      const string = `${input}`;
      let length = 0;
      for (let i = 0; i < string.length; ) {
        const cp = scalarAt(string, i);
        length += width(cp);
        i += cp > 0xffff ? 2 : 1;
      }
      const bytes = new Uint8Array(length);
      for (let i = 0, offset = 0; i < string.length; ) {
        const cp = scalarAt(string, i);
        write(bytes, offset, cp);
        offset += width(cp);
        i += cp > 0xffff ? 2 : 1;
      }
      return bytes;
    }

    // Writes only whole characters; `read` counts UTF-16 units consumed.
    encodeInto(source, destination) {
      const string = `${source}`;
      if (!(destination instanceof Uint8Array))
        throw new TypeError("TextEncoder.encodeInto destination must be a Uint8Array");
      let read = 0,
        written = 0;
      while (read < string.length) {
        const cp = scalarAt(string, read);
        const size = width(cp);
        if (written + size > destination.length) break;
        write(destination, written, cp);
        written += size;
        read += cp > 0xffff ? 2 : 1;
      }
      return { read, written };
    }
  }

  const UTF8_LABELS = new Set([
    "unicode-1-1-utf-8",
    "unicode11utf8",
    "unicode20utf8",
    "utf-8",
    "utf8",
    "x-unicode20utf8",
  ]);

  class TextDecoder {
    #fatal;
    #ignoreBOM;
    #doNotFlush = false;
    #bomSeen = false;
    #needed = 0;
    #seen = 0;
    #point = 0;
    #lower = 0x80;
    #upper = 0xbf;
    #pending = new Uint8Array(0);

    constructor(label = "utf-8", options = undefined) {
      const name = `${label}`
        .replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, "")
        .replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32));
      // WebIDL converts every argument, options included, before the label lookup.
      const settings = dictionary(options, "TextDecoder options");
      this.#fatal = Boolean(settings.fatal);
      this.#ignoreBOM = Boolean(settings.ignoreBOM);
      if (!UTF8_LABELS.has(name))
        throw new RangeError(
          `TextDecoder: unsupported encoding "${label}"; only UTF-8 is available`,
        );
    }

    get encoding() {
      return "utf-8";
    }

    get fatal() {
      return this.#fatal;
    }

    get ignoreBOM() {
      return this.#ignoreBOM;
    }

    #reset() {
      this.#needed = this.#seen = this.#point = 0;
      this.#lower = 0x80;
      this.#upper = 0xbf;
    }

    decode(input = undefined, options = undefined) {
      let bytes;
      if (input === undefined) bytes = new Uint8Array(0);
      else if (input instanceof ArrayBuffer) bytes = new Uint8Array(input);
      else if (ArrayBuffer.isView(input))
        bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
      else
        throw new TypeError(
          "TextDecoder.decode input must be an ArrayBuffer or ArrayBuffer view",
        );
      const stream = Boolean(dictionary(options, "decode options").stream);
      // A call after a flushing one starts a new stream.
      if (!this.#doNotFlush) {
        this.#reset();
        this.#bomSeen = false;
        this.#pending = new Uint8Array(0);
      } else if (this.#pending.length) {
        const queued = new Uint8Array(this.#pending.length + bytes.length);
        queued.set(this.#pending);
        queued.set(bytes, this.#pending.length);
        bytes = queued;
      }
      this.#pending = new Uint8Array(0);
      this.#doNotFlush = stream;
      const previousBOMSeen = this.#bomSeen;
      const parts = [];
      let units = [];
      const emit = (cp) => {
        if (!this.#ignoreBOM && !this.#bomSeen) {
          this.#bomSeen = true;
          if (cp === 0xfeff) return;
        }
        if (cp > 0xffff) {
          const offset = cp - 0x10000;
          units.push(0xd800 + (offset >> 10), 0xdc00 + (offset & 0x3ff));
        } else {
          units.push(cp);
        }
        if (units.length >= 0x2000) {
          parts.push(String.fromCharCode.apply(null, units));
          units = [];
        }
      };
      let i = 0;
      const error = () => {
        if (this.#fatal) {
          // A fatal processing result throws without flushing the input queue
          // or changing stream ownership. Copy unread bytes, never retain a
          // caller-owned view that can be changed before the next decode.
          if (stream) this.#pending = bytes.slice(i);
          this.#bomSeen = previousBOMSeen;
          throw new TypeError("TextDecoder: the encoded data is not valid UTF-8");
        }
        emit(REPLACEMENT);
      };
      for (; i < bytes.length; ) {
        const byte = bytes[i];
        if (this.#needed === 0) {
          ++i;
          if (byte <= 0x7f) {
            emit(byte);
          } else if (byte >= 0xc2 && byte <= 0xdf) {
            this.#needed = 1;
            this.#point = byte & 0x1f;
          } else if (byte >= 0xe0 && byte <= 0xef) {
            if (byte === 0xe0) this.#lower = 0xa0;
            if (byte === 0xed) this.#upper = 0x9f;
            this.#needed = 2;
            this.#point = byte & 0xf;
          } else if (byte >= 0xf0 && byte <= 0xf4) {
            if (byte === 0xf0) this.#lower = 0x90;
            if (byte === 0xf4) this.#upper = 0x8f;
            this.#needed = 3;
            this.#point = byte & 0x7;
          } else {
            error();
          }
          continue;
        }
        if (byte < this.#lower || byte > this.#upper) {
          // Replace the truncated sequence, then reprocess this byte.
          this.#reset();
          error();
          continue;
        }
        ++i;
        this.#lower = 0x80;
        this.#upper = 0xbf;
        this.#point = (this.#point << 6) | (byte & 0x3f);
        if (++this.#seen === this.#needed) {
          const cp = this.#point;
          this.#reset();
          emit(cp);
        }
      }
      if (!stream && this.#needed !== 0) {
        this.#reset();
        error();
      }
      if (units.length) parts.push(String.fromCharCode.apply(null, units));
      return parts.join("");
    }
  }

  for (const [name, value] of [
    ["TextEncoder", TextEncoder],
    ["TextDecoder", TextDecoder],
  ]) {
    Object.defineProperty(value.prototype, Symbol.toStringTag, {
      value: name,
      configurable: true,
    });
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      configurable: true,
    });
  }
})();
