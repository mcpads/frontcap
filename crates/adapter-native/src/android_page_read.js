// A library object is an observation, not application readiness. Do not invoke a page getter.
JSON.stringify({
  href: location.href.slice(0, 2048),
  ready_state: document.readyState,
  visibility: document.visibilityState,
  focused: document.hasFocus(),
  performance_ms: performance.now(),
  date_ms: Date.now(),
  media: globalThis.__frontcapMedia?.snapshot() ?? null,
  navigation: globalThis.__frontcapNavigation?.snapshot() ?? null,
  starting_randomness: globalThis.__frontcapStartingRandomness?.snapshot() ?? null,
  canvas_kit: globalThis.__frontcapFrames ? {
    document: __frontcapFrames.receipt().document,
    published: (() => {
      const value = Object.getOwnPropertyDescriptor(globalThis, 'flutterCanvasKit')?.value;
      return value !== null && typeof value === 'object';
    })(),
  } : null,
  timer_activity: globalThis.__frontcapTimers?.snapshot() ?? null,
  frame_activity: globalThis.__frontcapFrames ? {
    document: __frontcapFrames.receipt().document, ...__frontcapFrames.snapshot(),
  } : null,
  page_events: globalThis.__frontcapEvents && globalThis.__frontcapFrames ? {
    document: __frontcapFrames.receipt().document, ...__frontcapEvents.snapshot(),
  } : null,
})
