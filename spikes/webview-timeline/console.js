// Platform-installed main-realm console observation; never traverses logged objects.
export function observeConsole(scope, document) {
  const entries = [], installed = new Map();
  let received = 0, failure = null;
  const methods = ['log', 'debug', 'info', 'warn', 'error'];
  const console = scope.console;
  function record(level, args) {
    if (failure !== null) return;
    try {
      let text = '', truncated = false;
      for (const value of args) {
        if (text) text += ' ';
        const type = typeof value;
        // Only primitives are converted; no getters, proxies, object traversal or custom toString.
        text += value === null ? 'null' :
          ['string', 'number', 'boolean', 'undefined', 'bigint'].includes(type) ? String(value) : `[${type}]`;
        if (text.length > 1024) { truncated = true; break; }
      }
      let end = Math.min(text.length, 1024);
      if (end < text.length && /[\uD800-\uDBFF]/.test(text[end - 1])) end--;
      text = text.slice(0, end);
      for (let i = 0; i < text.length; i++) {
        const code = text.charCodeAt(i);
        if (code >= 0xd800 && code <= 0xdbff) {
          const next = text.charCodeAt(++i);
          if (!(next >= 0xdc00 && next <= 0xdfff)) throw Error('Invalid UTF-16');
        } else if (code >= 0xdc00 && code <= 0xdfff) throw Error('Invalid UTF-16');
      }
      const elapsedMs = scope.performance.now();
      if (!Number.isFinite(elapsedMs) || elapsedMs < 0 || received === Number.MAX_SAFE_INTEGER)
        throw Error('Invalid observation clock or sequence');
      entries.push({sequence:++received, elapsedMs, level, text, truncated});
      if (entries.length > 32) entries.shift();
    } catch (_) { failure = 'Console observation failed; later calls are not covered'; }
  }
  for (const level of methods) {
    try {
      const descriptor = Object.getOwnPropertyDescriptor(console, level);
      if (!descriptor || typeof descriptor.value !== 'function') throw Error('Unsupported console descriptor');
      const original = descriptor.value;
      function observed(...args) {
        try { return Reflect.apply(original, this, args); }
        finally { record(level, args); }
      }
      Object.defineProperty(console, level, {...descriptor, value:observed});
      installed.set(level, observed);
    } catch (_) { failure = 'Console method unavailable; coverage is incomplete'; }
  }
  return Object.freeze({snapshot() {
    try {
      if (scope.console !== console) failure = 'Console object replaced; coverage is incomplete';
      for (const [level, observed] of installed) {
        if (Object.getOwnPropertyDescriptor(console, level)?.value !== observed)
          failure = 'Console method replaced; coverage is incomplete';
      }
    } catch (_) { failure = 'Console inspection failed; coverage is incomplete'; }
    return {document, received, dropped:received - entries.length, failure,
      entries:entries.map(entry => ({...entry}))};
  }});
}
