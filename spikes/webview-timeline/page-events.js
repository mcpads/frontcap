// Passive window-message and visibility evidence; never releases or suppresses delivery.
export function observePageEvents(scope) {
  const events = [];
  let received = 0;
  let failure = null;
  function bounded(text, limit) {
    let end = Math.min(text.length, limit);
    if (end < text.length && /[\uD800-\uDBFF]/.test(text[end - 1])) end--;
    const value = text.slice(0, end);
    // Preserve exact text or report lost coverage; do not silently repair a payload.
    for (let i = 0; i < value.length; i++) {
      const code = value.charCodeAt(i);
      if (code >= 0xd800 && code <= 0xdbff) {
        const next = value.charCodeAt(++i);
        if (!(next >= 0xdc00 && next <= 0xdfff)) throw Error('Unpaired UTF-16');
      } else if (code >= 0xdc00 && code <= 0xdfff) throw Error('Unpaired UTF-16');
    }
    return value;
  }
  function record(sample) {
    if (failure !== null) return;
    try {
      const elapsedMs = scope.performance.now();
      if (!Number.isFinite(elapsedMs) || elapsedMs < 0 || received === Number.MAX_SAFE_INTEGER)
        throw Error('Invalid observation clock or sequence');
      const event = sample();
      events.push({sequence:++received, elapsedMs, ...event});
      if (events.length > 8) events.shift();
    } catch (_) {
      failure = 'Page event observation failed; subsequent events are not covered';
    }
  }
  scope.addEventListener('message', event => record(() => {
    const data = event.data;
    const text = typeof data === 'string' ? data : null;
    const origin = event.origin;
    return {kind:'message', origin:bounded(origin, 32), originTruncated:origin.length > 32,
      dataType:typeof data, data:text === null ? null : bounded(text, 64),
      truncated:text !== null && text.length > 64};
  }));
  scope.document.addEventListener('visibilitychange', () => record(() =>
    ({kind:'visibility', state:scope.document.visibilityState})));
  return Object.freeze({snapshot:() => ({received, dropped:received-events.length,
    events:events.map(event => ({...event})), ...(failure === null ? {} : {failure})})});
}
