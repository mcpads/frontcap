// Passive completion evidence, not a readiness predicate or network admission gate.
(() => {
  if (Object.hasOwn(globalThis, '__frontcapResources')) throw Error('Resource observer already installed');
  let received = 0, errors = 0, bufferFull = 0;
  const latest = [];
  const counts = {fetch:0, xmlhttprequest:0, script:0, img:0, css:0, other:0};
  // The timeline is installed next; read its clock at observation time.
  const now = () => performance.now();
  function accept(entries) {
    for (const entry of entries) {
      try {
        const kind = Object.hasOwn(counts, entry.initiatorType) ? entry.initiatorType : 'other';
        const path = new URL(entry.name, document.baseURI).pathname;
        received++;
        counts[kind]++;
        latest.push({sequence:received, kind, path:path.slice(0, 96), pathTruncated:path.length > 96,
          observedElapsedMs:now(), resourceStartMs:entry.startTime, resourceDurationMs:entry.duration});
        if (latest.length > 2) latest.shift();
      } catch (_) { errors++; }
    }
  }
  let observer = null;
  if (typeof PerformanceObserver === 'function') {
    try {
      observer = new PerformanceObserver(list => accept(list.getEntries()));
      observer.observe({type:'resource', buffered:true});
    } catch (_) { observer?.disconnect(); observer = null; errors++; }
  }
  performance.addEventListener('resourcetimingbufferfull', () => { bufferFull++; });
  Object.defineProperty(globalThis, '__frontcapResources', {value:Object.freeze({
    snapshot:() => {
      // Include queued completions even when the observer callback has not run yet.
      if (observer) accept(observer.takeRecords());
      return {supported:observer !== null, documentState:document.readyState,
        fontsStatus:document.fonts?.status ?? null, pendingResources:null,
        received, counts:{...counts}, errors, bufferFull, dropped:received-latest.length,
        latest:latest.map(row => ({...row}))};
    },
  }), configurable:false, writable:false});
})();
