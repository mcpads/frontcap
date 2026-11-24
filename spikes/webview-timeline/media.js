// Native media remains outside the controlled page clock, including in muted output mode.
export function observeMedia(scope, {mutedOutput = false} = {}) {
  const hostNow = scope.performance.now.bind(scope.performance);
  const started = hostNow();
  const prototype = scope.HTMLMediaElement?.prototype;
  const entries = [];
  const known = new WeakMap();
  const sources = new Map();
  let sourceLimitReached = false;
  const events = [];
  const flow = new Map();
  const flowKinds = new Set(['play-call', 'pause-call', 'playing', 'pause', 'ended', 'error']);
  let unattributed = 0;
  let received = 0, omitted = 0, faults = 0;
  if (mutedOutput && prototype) {
    const muted = Object.getOwnPropertyDescriptor(prototype, 'muted');
    if (!muted?.set || !muted.configurable) throw Error('Native media mute unavailable');
    Object.defineProperty(prototype, 'muted', {...muted, set() {
      Reflect.apply(muted.set, this, [true]);
    }});
  }
  const finite = value => Number.isFinite(value) ? value : null;
  // Identity only: do not export URLs, query credentials, data payloads or audio bytes.
  // These ordinal IDs are meaningful only within the observed document.
  function sourceId(value) {
    if (typeof value !== 'string' || value.length === 0) return null;
    if (value.length > 4096) { sourceLimitReached = true; return null; }
    if (sources.has(value)) return sources.get(value);
    if (sources.size === 64) { sourceLimitReached = true; return null; }
    const id = sources.size + 1;
    sources.set(value, id);
    return id;
  }
  function state(e) {
    return {sourceId:sourceId(e.currentSrc), requestedSourceId:sourceId(e.src),
      currentTime:finite(e.currentTime), duration:finite(e.duration),
      paused:e.paused, ended:e.ended, seeking:e.seeking, loop:e.loop,
      readyState:e.readyState, networkState:e.networkState,
      muted:e.muted, volume:finite(e.volume), playbackRate:finite(e.playbackRate),
      error:e.error?.code ?? null};
  }
  function record(id, kind, element) {
    const atEvent = state(element);
    events.push({sequence:++received, id, kind,
      hostMs:hostNow()-started, pageMs:scope.performance.now(), state:atEvent});
    if (events.length > 8) events.shift();
    if (flowKinds.has(kind)) {
      // A request can name a new source before currentSrc has switched.
      const source = kind === 'play-call'
        ? atEvent.requestedSourceId ?? atEvent.sourceId : atEvent.sourceId;
      if (source === null) { unattributed++; return; }
      if (!flow.has(source)) flow.set(source, {});
      const summary = flow.get(source);
      const previous = summary[kind];
      summary[kind] = {count:(previous?.count ?? 0) + 1, lastSequence:received};
    }
  }
  function track(element) {
    if (known.has(element)) return known.get(element);
    if (entries.length === 8) {
      known.set(element, null);
      omitted++;
      return null;
    }
    const id = entries.length + 1;
    known.set(element, id);
    entries.push({id, element});
    for (const name of ['playing', 'pause', 'ended', 'error', 'loadedmetadata', 'seeking', 'seeked',
      'ratechange', 'durationchange', 'emptied', 'waiting']) {
      element.addEventListener(name, () => {
        try { record(id, name, element); } catch { faults++; }
      });
    }
    return id;
  }
  if (prototype) {
    for (const name of ['play', 'pause']) {
      const descriptor = Object.getOwnPropertyDescriptor(prototype, name);
      const original = descriptor.value;
      Object.defineProperty(prototype, name, {...descriptor, value:function(...args) {
        // Explicit output policy: preserve native playback, Promise and completion behavior.
        if (mutedOutput && name === 'play') this.muted = true;
        // Never consume or replace a play Promise, including its rejection.
        try {
          const id = track(this);
          if (id !== null) record(id, name + '-call', this);
        } catch { faults++; }
        return Reflect.apply(original, this, args);
      }});
    }
  }
  return Object.freeze({snapshot() {
    const players = entries.map(({id, element}) => ({id, ...state(element)}));
    return {available:!!prototype, controlled:false, mutedOutput, hostMs:hostNow()-started,
      document:scope.__frontcapDocument ?? scope.__frontcapFrames?.receipt().document ?? null, sourceCount:sources.size, sourceLimitReached,
      received, dropped:received-events.length, omitted, faults,
      players,
      flow:{unattributed, sources:Array.from(flow, ([sourceId, summary]) => ({sourceId,
        events:Object.fromEntries(Object.entries(summary).map(([kind, count]) => [kind, {...count}]))}))},
      events:events.map(event => ({...event, state:{...event.state}}))};
  }});
}
