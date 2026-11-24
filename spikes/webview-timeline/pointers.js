// Capture delivery without cancelling events or injecting page input.
export function observePointers(scope) {
  const active = new Set(), events = [];
  let received = 0, maximumActive = 0;
  for (const kind of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel']) {
    scope.addEventListener(kind, event => {
      if (kind === 'pointerdown') active.add(event.pointerId);
      if (kind === 'pointerup' || kind === 'pointercancel') active.delete(event.pointerId);
      maximumActive = Math.max(maximumActive, active.size);
      events.push({sequence:++received, kind, id:event.pointerId, trusted:event.isTrusted,
        x:event.clientX, y:event.clientY, pageMs:scope.performance.now(), active:active.size});
      if (events.length > 8) events.shift();
    }, {capture:true, passive:true});
  }
  return Object.freeze({snapshot:() => ({received, dropped:received-events.length,
    active:active.size, maximumActive, events:events.map(event => ({...event}))})});
}
