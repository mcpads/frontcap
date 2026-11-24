// Document-local navigation evidence. No polling, app timers, or history-state payloads.
export function observeNavigation(scope, documentId) {
  const events = [];
  let received = 0;
  let failure = null;
  const wrappers = {};
  const apply = Reflect.apply;
  const now = scope.performance.now.bind(scope.performance);
  function bounded(text, limit) {
    let end = Math.min(text.length, limit);
    if (end < text.length && /[\uD800-\uDBFF]/.test(text[end - 1])) end--;
    return text.slice(0, end);
  }
  function position() {
    const href = scope.location.href;
    return {href:bounded(href, 2048), hrefTruncated:href.length > 2048,
      focused:scope.document.hasFocus()};
  }
  function failed(error) {
    if (failure !== null) return;
    try { failure = bounded(String(error), 256); }
    catch { failure = 'Navigation observation failed'; }
  }
  function record(kind, event = null) {
    if (failure !== null) return;
    try {
      const sample = {elapsedMs:now(), ...position(), kind,
        trusted:event === null ? null : event.isTrusted === true,
        persisted:kind === 'pageshow' || kind === 'pagehide' ? event.persisted === true : null};
      events.push({sequence:++received, ...sample});
      if (events.length > 8) events.shift();
    } catch (error) {
      // A failed observer must not turn an already-applied history change into an app exception.
      failed(error);
    }
  }
  for (const kind of ['popstate', 'hashchange', 'pageshow', 'pagehide', 'focus', 'blur']) {
    scope.addEventListener(kind, event => record(kind, event));
  }
  for (const [method, kind] of [['pushState', 'push_state'], ['replaceState', 'replace_state']]) {
    const original = scope.history[method];
    const wrapper = function (...args) {
      const result = apply(original, this, args);
      record(kind);
      return result;
    };
    try {
      if (Reflect.set(scope.history, method, wrapper)) wrappers[method] = wrapper;
    } catch (error) { failed(error); }
  }
  return Object.freeze({snapshot() {
    let current = null;
    let historyObserved = false;
    try {
      current = position();
      historyObserved = ['pushState', 'replaceState'].every(method =>
        wrappers[method] !== undefined && scope.history[method] === wrappers[method]);
    } catch (error) { failed(error); }
    return {document:documentId, current, failure, historyObserved,
      received, dropped:received - events.length, events:events.map(event => ({...event}))};
  }});
}
