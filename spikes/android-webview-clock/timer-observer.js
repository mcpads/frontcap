// Observe main-realm function timers without replacing the engine's scheduling or ID space.
export function observeTimers(scope, documentId) {
  const timers = new Map();
  const wrappers = {};
  const apply = Reflect.apply;
  const now = scope.performance.now.bind(scope.performance);
  let registered = 0, invoked = 0, canceled = 0, executing = 0;
  let untrackedRegistrations = 0, uncertainCancellations = 0;
  let failure = null;
  function observe(action) {
    if (failure !== null) return;
    try { action(); } catch { failure = 'Timer observation failed'; }
  }
  for (const [method, kind] of [['setTimeout', 'timeout'], ['setInterval', 'interval']]) {
    const original = scope[method];
    const wrapper = function (...args) {
      const callback = args[0];
      let id, entry;
      if (typeof callback === 'function') {
        args[0] = function (...values) {
          invoked++;
          observe(() => {
            if (entry && timers.get(id) === entry) {
              entry.calls++;
              if (kind === 'timeout') timers.delete(id);
            }
          });
          executing++;
          try { return apply(callback, this, values); }
          finally { executing--; }
        };
      }
      // Conversion, clamping, CSP checks, callback nesting and native exceptions stay native.
      id = apply(original, this, args);
      if (id === 0) return id; // The platform did not create a timer (for example, CSP rejection).
      observe(() => {
        const registration = ++registered;
        // Native IDs may be reused after an opaque cancellation or an unobserved expiry.
        timers.delete(id);
        if (typeof callback !== 'function' || !Number.isInteger(id) || id < 1 || id > 0x7fffffff || timers.size >= 4096) {
          untrackedRegistrations++; return;
        }
        const delay = args.length < 2 ? 0 : args[1];
        entry = {id, registration, kind, registeredMs:now(),
          requestedDelayMs:typeof delay === 'number' && Number.isFinite(delay) ? delay : null, calls:0};
        timers.set(id, entry);
      });
      return id;
    };
    scope[method] = wrappers[method] = wrapper;
  }
  for (const method of ['clearTimeout', 'clearInterval']) {
    const original = scope[method];
    const wrapper = function (...args) {
      const result = apply(original, this, args);
      observe(() => {
        const id = args[0];
        // Primitive conversion has no consumer callbacks. Never invoke an object's conversion twice.
        if (id !== null && (typeof id === 'object' || typeof id === 'function' || typeof id === 'symbol' || typeof id === 'bigint')) {
          uncertainCancellations++; return;
        }
        if (timers.delete(Number(id) | 0)) canceled++;
      });
      return result;
    };
    scope[method] = wrappers[method] = wrapper;
  }
  return Object.freeze({snapshot() {
    let sampledMs = null, methodsObserved = false;
    try {
      sampledMs = now();
      methodsObserved = Object.keys(wrappers).every(method => scope[method] === wrappers[method]);
    } catch { failure = 'Timer observation failed'; }
    const rows = [...timers.values()].slice(0, 32).map(entry => ({...entry}));
    return {document:documentId, sampledMs, methodsObserved, failure, registered, invoked, canceled,
      executing, untrackedRegistrations, uncertainCancellations, tracked:timers.size,
      omitted:timers.size - rows.length, timers:rows};
  }});
}
