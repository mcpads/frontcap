// Source-owned animation callback admission experiment. Install before app startup.
// Native composition, CSS animations, workers and other producers remain outside this gate.
(() => {
  if (Object.hasOwn(globalThis, '__frontcapFrames')) throw Error('Frame gate already installed');
  const request = globalThis.requestAnimationFrame.bind(globalThis);
  const cancel = globalThis.cancelAnimationFrame.bind(globalThis);
  const callbacks = new Map();
  let nextId = 0;
  let active = null;
  let lastTimestamp = null;
  let delivered = 0;
  globalThis.requestAnimationFrame = callback => {
    if (typeof callback !== 'function') throw new TypeError('Animation callback must be callable');
    const id = ++nextId;
    callbacks.set(id, {callback, nativeId:null});
    return id;
  };
  globalThis.cancelAnimationFrame = id => {
    const entry = callbacks.get(Number(id));
    if (!entry) return;
    if (entry.nativeId !== null) cancel(entry.nativeId);
    callbacks.delete(Number(id));
  };
  function snapshot() {
    return {pending:callbacks.size, dispatching:active !== null, lastTimestamp, delivered};
  }
  function deliver(timestamp) {
    if (!Number.isFinite(timestamp) || timestamp < 0 ||
        (lastTimestamp !== null && timestamp < lastTimestamp)) {
      throw new RangeError('Frame timestamp must be finite, nonnegative and monotonic');
    }
    if (active !== null) throw Error('Frame delivery already pending');
    const batch = {timestamp, delivered:0, errors:[], nativeTimestamp:null};
    active = batch;
    lastTimestamp = timestamp;
    // Use actual native callbacks so the engine retains its callback/microtask boundaries.
    // Registrations made by a delivered callback stay queued for the next explicit frame.
    for (const [id, entry] of [...callbacks]) {
      entry.nativeId = request(nativeTimestamp => {
        callbacks.delete(id);
        batch.nativeTimestamp = nativeTimestamp;
        batch.delivered++;
        delivered++;
        try { entry.callback.call(globalThis, timestamp); }
        catch (error) {
          batch.errors.push(String(error));
          throw error; // Preserve the browser's ordinary uncaught-error reporting.
        }
      });
    }
    return new Promise(resolve => {
      request(nativeTimestamp => {
        batch.nativeTimestamp = nativeTimestamp;
        active = null;
        resolve({...batch, state:snapshot()});
      });
    });
  }
  // Only the current document's latest producer receipt is retained here. The controller
  // must durably retain older completions before admitting the next effect.
  const stringify = JSON.stringify.bind(JSON);
  const parse = JSON.parse.bind(JSON);
  const random = globalThis.crypto?.getRandomValues.bind(globalThis.crypto);
  let documentId = null;
  let last = null;
  let legacyUsed = false;
  function identity() {
    if (documentId === null) {
      if (!random) throw Error('Document identity entropy unavailable');
      const bytes = random(new Uint8Array(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const hex = [...bytes].map(x => x.toString(16).padStart(2, '0')).join('');
      documentId = `${hex.slice(0,8)}-${hex.slice(8,12)}-${hex.slice(12,16)}-${hex.slice(16,20)}-${hex.slice(20)}`;
    }
    return documentId;
  }
  function receipt() {
    return parse(stringify({document:identity(), legacyUsed, last}));
  }
  function dispatch(command) {
    if (!command || Object.keys(command).sort().join(',') !== 'document,id,timestamp' ||
        command.document !== identity() || !Number.isSafeInteger(command.id) || command.id < 1 ||
        !Number.isFinite(command.timestamp) || command.timestamp < 0) {
      throw Error('Invalid frame request identity or timestamp');
    }
    if (legacyUsed) throw Error('Frame gate already used without request receipts');
    if (last && command.id === last.request.id) {
      if (command.timestamp !== last.request.timestamp) throw Error('Frame request identity conflict');
      return receipt();
    }
    if (command.id !== (last ? last.request.id + 1 : 1)) throw Error('Frame request is not next');
    if (last && last.state !== 'completed') throw Error('Previous frame request unresolved');
    if (lastTimestamp !== null && command.timestamp < lastTimestamp) throw Error('Frame timestamp is not monotonic');
    last = {request:{document:command.document,id:command.id,timestamp:command.timestamp},state:'pending',result:null,error:null};
    const admitted = last;
    try {
      deliver(command.timestamp).then(result => {
        admitted.result = result;
        admitted.state = 'completed';
      }, error => {
        admitted.error = String(error);
        admitted.state = 'unresolved';
      });
    } catch (error) {
      // Scheduling can fail after some callbacks were registered; do not roll back or replay.
      admitted.error = String(error);
      admitted.state = 'unresolved';
    }
    return receipt();
  }
  function advance(timestamp) {
    if (last !== null) throw Error('Use receipt-bound frame requests after dispatch');
    legacyUsed = true;
    return deliver(timestamp);
  }
  Object.defineProperty(globalThis, '__frontcapFrames', {
    value:Object.freeze({snapshot, advance, receipt, dispatch}), configurable:false, writable:false,
  });
})();
