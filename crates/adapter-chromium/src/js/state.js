// Main world, in every frame, before any page script: the channel an app registers its state providers on
// (`frontcapState.register({id, read, write?, description?, max_ms?})`). The platform reads and writes them
// from its isolated world through events whose names derive from the launch, so a page cannot forge a
// request, and a provider is reached only through the document that registered it. Its JSON value is what
// `read()` returns, and `write(value)` sets it; either may return a promise, whose outcome the platform
// asks for by the token it named the request with. `max_ms` is the virtual time its promises may need.
(() => {
  const REQUEST = "__FRONTCAP_STATE_REQUEST__";
  const REPLY = "__FRONTCAP_STATE_REPLY__";
  const listen = EventTarget.prototype.addEventListener;
  const dispatch = EventTarget.prototype.dispatchEvent;
  const Custom = CustomEvent;
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const text = String;
  const apply = Reflect.apply;
  const ID = /^[A-Za-z0-9_.-]{1,64}$/;
  const test = RegExp.prototype.test;
  const providers = new Map();
  const get = Map.prototype.get.bind(providers);
  const set = Map.prototype.set.bind(providers);
  const entries = Map.prototype.entries.bind(providers);
  const isInteger = Number.isInteger;
  const register = (provider) => {
    if (provider === null || typeof provider !== "object") throw new TypeError("frontcapState.register takes a provider object");
    const { id, read, write, description, max_ms } = provider;
    if (typeof id !== "string" || !test.call(ID, id)) throw new TypeError("a provider id is 1 to 64 letters, digits, '.', '_' or '-'");
    if (typeof read !== "function") throw new TypeError("a provider reads its state with read()");
    if (write !== undefined && typeof write !== "function") throw new TypeError("a provider's write is a function");
    if (max_ms !== undefined && !(isInteger(max_ms) && max_ms >= 0 && max_ms <= 10000)) throw new TypeError("a provider's max_ms is an integer from 0 to 10000");
    set(id, { provider, read, write, description: typeof description === "string" ? description.slice(0, 256) : "", max_ms: max_ms === undefined ? 0 : max_ms });
  };
  // Outcomes of promises the platform's requests started, by token; the oldest are dropped past a bound.
  const outcomes = new Map();
  const outcome = Map.prototype.get.bind(outcomes);
  const keep = Map.prototype.set.bind(outcomes);
  const drop = Map.prototype.delete.bind(outcomes);
  const tokens = Map.prototype.keys.bind(outcomes);
  const count = Reflect.getOwnPropertyDescriptor(Map.prototype, "size").get;
  const then = (promise, onSettled, onFailed) => apply(promise.then, promise, [onSettled, onFailed]);
  const track = (token, promise, json) => {
    if (typeof token !== "string") throw new TypeError("an asynchronous provider needs the request's token");
    if (apply(count, outcomes, []) >= 64) drop(tokens().next().value);
    const entry = { state: "pending" };
    keep(token, entry);
    then(promise, (value) => {
      entry.state = "settled";
      if (json) {
        const encoded = stringify(value);
        if (encoded === undefined) { entry.state = "failed"; entry.error = "the promise settled with no JSON value"; } else entry.value = encoded;
      }
    }, (err) => { entry.state = "failed"; entry.error = text(err && err.message || err); });
  };
  Object.defineProperty(window, "frontcapState", { value: Object.freeze({ register }), configurable: false, writable: false, enumerable: false });
  const thenable = (value) => value !== null && (typeof value === "object" || typeof value === "function") && typeof value.then === "function";
  const handle = (request) => {
    if (request.op === "list") {
      const list = [];
      for (const [id, p] of entries()) list.push({ id, writable: p.write !== undefined, description: p.description, max_ms: p.max_ms });
      return { providers: list };
    }
    if (request.op === "outcome") {
      const entry = outcome(request.token);
      if (entry === undefined) return { error: `no state request ${request.token} is pending in this document` };
      return { state: entry.state, value: entry.value, error: entry.error };
    }
    const p = get(request.id);
    if (p === undefined) return { error: `no state provider ${request.id} is registered in this document` };
    if (request.op === "read") {
      const value = apply(p.read, p.provider, []);
      if (thenable(value)) {
        track(request.token, value, true);
        return { pending: request.token };
      }
      const json = stringify(value);
      return json === undefined ? { error: `${request.id}'s read() returned no JSON value` } : { value: json };
    }
    if (request.op === "write") {
      if (p.write === undefined) return { error: `state provider ${request.id} is read only` };
      // Called: whatever it did is done, even when it then fails or answers late.
      const result = apply(p.write, p.provider, [parse(request.value)]);
      if (thenable(result)) {
        track(request.token, result, false);
        return { written: true, pending: request.token };
      }
      return { written: true };
    }
    return { error: "unknown state request" };
  };
  listen.call(document, REQUEST, (e) => {
    let request;
    try { request = parse(e.detail); } catch (err) { return; }
    let answer;
    try { answer = handle(request); } catch (err) { answer = { error: text(err) }; }
    dispatch.call(document, new Custom(REPLY, { detail: stringify(answer) }));
  });
})();
