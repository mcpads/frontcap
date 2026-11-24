// Main world, in every frame, before any page script: randomness from the profile's seed (mulberry32).
// `Math.random` and the Web Crypto random sources draw from separate streams of the same seed. The
// platform's crypto is deterministic by design and therefore not cryptographically secure.
(() => {
  const stream = (seed) => {
    let a = seed >>> 0;
    return () => {
      a = (a + 0x6d2b79f5) >>> 0;
      let t = a;
      t = Math.imul(t ^ (t >>> 15), t | 1);
      t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
      return (t ^ (t >>> 14)) >>> 0;
    };
  };
  const seed = __FRONTCAP_SEED__;
  const forMath = stream(seed);
  Math.random = function random() { return forMath() / 4294967296; };
  // What draws randomness the seed cannot reach is announced to the top frame's observer as taint (the
  // same channel as dialogs.js), at the instant of the call. A frame of another origin cannot reach it.
  let top = null;
  try {
    top = window.top.document;
  } catch (e) {
    // Another origin's frame.
  }
  const EVENT = "__FRONTCAP_DIALOG_EVENT__";
  const dispatch = EventTarget.prototype.dispatchEvent;
  const Custom = CustomEvent;
  const stringify = JSON.stringify;
  const announce = (taint) => {
    if (top === null) return;
    try {
      dispatch.call(top, new Custom(EVENT, { detail: stringify({ taint }) }));
    } catch (e) {
      // A document being torn down.
    }
  };
  // A worker the page starts, named as the browser names its target: announced when it is constructed,
  // the page's instant, rather than when the browser announces the target, which waits for its script's
  // network answer.
  for (const [name, kind] of [["Worker", "worker"], ["SharedWorker", "shared_worker"]]) {
    const Original = window[name];
    if (typeof Original !== "function") continue;
    const Wrapped = function (...args) {
      if (new.target === undefined) return Reflect.apply(Original, this, args);
      const worker = Reflect.construct(Original, args, new.target === Wrapped ? Original : new.target);
      try {
        announce(`${kind} ${new URL(String(args[0]), document.baseURI).href}`);
      } catch (e) {
        // Constructed, so its URL parsed; nothing to announce otherwise.
      }
      return worker;
    };
    Object.defineProperty(Wrapped, "name", { value: name });
    Object.defineProperty(Wrapped, "length", { value: Original.length });
    Wrapped.prototype = Original.prototype;
    Object.defineProperty(Original.prototype, "constructor", { value: Wrapped, writable: true, configurable: true, enumerable: false });
    Object.setPrototypeOf(Wrapped, Object.getPrototypeOf(Original));
    Object.defineProperty(window, name, { value: Wrapped, writable: true, configurable: true, enumerable: false });
  }
  if (typeof Crypto !== "function") return;
  const forCrypto = stream(seed ^ 0x9e3779b9);
  const integerViews = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, BigInt64Array, BigUint64Array];
  const fill = (bytes) => {
    for (let i = 0; i < bytes.length; i += 4) {
      const r = forCrypto();
      for (let k = 0; k < 4 && i + k < bytes.length; k++) bytes[i + k] = (r >>> (8 * k)) & 0xff;
    }
  };
  Crypto.prototype.getRandomValues = function getRandomValues(view) {
    if (!integerViews.some((T) => view instanceof T)) throw new TypeError("getRandomValues needs an integer typed array");
    if (view.byteLength > 65536) throw new DOMException("getRandomValues is limited to 65536 bytes", "QuotaExceededError");
    fill(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
    return view;
  };
  Crypto.prototype.randomUUID = function randomUUID() {
    const b = new Uint8Array(16);
    fill(b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  };
  // Web Crypto operations that draw randomness inside the engine cannot follow the seed.
  if (typeof SubtleCrypto !== "function") return;
  const nameOf = (algorithm) => String((typeof algorithm === "string" ? algorithm : algorithm && algorithm.name) || "").toUpperCase();
  const random = {
    generateKey: () => true,
    encrypt: (n) => n === "RSA-OAEP",
    wrapKey: (n) => n === "RSA-OAEP",
    sign: (n) => n === "ECDSA" || n === "RSA-PSS",
  };
  for (const [method, draws] of Object.entries(random)) {
    const original = SubtleCrypto.prototype[method];
    const argument = method === "wrapKey" ? 3 : 0;
    SubtleCrypto.prototype[method] = function (...args) {
      const algorithm = nameOf(args[argument]);
      if (draws(algorithm)) {
        announce(`crypto.subtle.${method} ${algorithm}`);
      }
      return original.apply(this, args);
    };
  }
})();
