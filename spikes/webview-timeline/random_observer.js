// Optional diagnostic after random initialization, before consumer code. No replay or stream reset.
(() => {
  if (Object.hasOwn(globalThis, '__frontcapRandomCalls')) throw Error('Random observer already installed');
  const first = [], tail = [];
  const callWindow = null; // Replaced only by the opt-in iOS composer.
  const callers = new Map();
  let windowCalls = 0, omittedCalls = 0;
  const counts = {math:0, bytes:0, uuid:0};
  let received = 0, errors = 0;
  const now = performance.now.bind(performance);
  function record(kind, detail) {
    counts[kind]++;
    received++;
    try {
      const stack = String(new Error().stack ?? '').split('\n').filter(line =>
        line !== 'Error' && !/^(record|randomObserved|bytesObserved|uuidObserved)@/.test(line) &&
        !/at (record|randomObserved|bytesObserved|uuidObserved) /.test(line)).join('\n');
      const row = {sequence:received, kind, elapsedMs:now(), ...detail,
        stack:stack.slice(0, 160), stackTruncated:stack.length > 160};
      if (callWindow && row.elapsedMs >= callWindow[0] && row.elapsedMs < callWindow[1]) {
        windowCalls++;
        // Ignore independently allocated origins; retain function/path/line context.
        const caller = stack.replace(/https?:\/\/[^/\s)]+/g, '<origin>').slice(0, 320);
        const key = kind + ':' + caller;
        const existing = callers.get(key);
        if (existing) existing.calls++;
        else if (callers.size < 16) callers.set(key, {kind, caller, truncated:stack.replace(/https?:\/\/[^/\s)]+/g, '<origin>').length > 320, calls:1});
        else omittedCalls++;
      }
      if (first.length < 4) first.push(row);
      else { tail.push(row); if (tail.length > 4) tail.shift(); }
    } catch (_) { errors++; } // Diagnostics must not turn a successful random call into an app error.
  }
  const random = Math.random;
  Math.random = function randomObserved() {
    const value = random.call(this);
    record('math', {value});
    return value;
  };
  if (typeof Crypto === 'function') {
    const bytes = Crypto.prototype.getRandomValues;
    const uuid = Crypto.prototype.randomUUID;
    if (typeof bytes === 'function') Crypto.prototype.getRandomValues = function bytesObserved(view) {
      const result = bytes.call(this, view);
      record('bytes', {byteLength:result.byteLength});
      return result;
    };
    if (typeof uuid === 'function') Crypto.prototype.randomUUID = function uuidObserved() {
      const result = uuid.call(this);
      record('uuid', {});
      return result;
    };
  }
  Object.defineProperty(globalThis, '__frontcapRandomCalls', {value:Object.freeze({
    snapshot:() => ({callWindow:callWindow ? {startMs:callWindow[0],endMs:callWindow[1],calls:windowCalls,omittedCalls,callers:Array.from(callers.values(), row => ({...row}))} : null, received, counts:{...counts}, errors, dropped:received-first.length-tail.length,
      first:first.map(row => ({...row})), tail:tail.map(row => ({...row}))}),
  }), configurable:false, writable:false});
})();
