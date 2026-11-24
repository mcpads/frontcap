import FakeTimers from '@sinonjs/fake-timers';

// Platform instrumentation for main-realm timers. Install before application code.
// Native messages, network, workers, audio and composition are not owned here.
export function installTimeline(scope, frames, {epochMs = 1700000000000, callbackLimit = 10000} = {}) {
  if (!Number.isSafeInteger(epochMs) || !Number.isSafeInteger(callbackLimit) || callbackLimit <= 0) {
    throw new TypeError('Invalid epoch or callback limit');
  }
  const clock = FakeTimers.withGlobal(scope).install({
    now:epochMs, shouldAdvanceTime:false,
    toFake:['setTimeout','clearTimeout','setInterval','clearInterval','Date','performance'],
  });
  const timers = new Map();
  const timeout = scope.setTimeout.bind(scope);
  const interval = scope.setInterval.bind(scope);
  const clearTimeout = scope.clearTimeout.bind(scope);
  const clearInterval = scope.clearInterval.bind(scope);
  let advancing = false;
  let failure = null;
  let callbacks = 0;
  let cancelledOnFailure = 0;

  function clear(id) {
    const entry = timers.get(id);
    if (!entry) return;
    (entry.periodic ? clearInterval : clearTimeout)(id);
    timers.delete(id);
  }
  function fail(error) {
    failure ??= String(error);
    cancelledOnFailure += timers.size;
    for (const id of timers.keys()) clear(id);
  }
  function schedule(callback, delay, args, periodic) {
    if (failure) throw Error('Timeline faulted; replace the document');
    if (typeof callback !== 'function') {
      fail('String timer handlers are not supported by this source-owned timeline');
      throw new TypeError(failure);
    }
    let id;
    const invoke = () => {
      if (!periodic) timers.delete(id);
      if (failure) return;
      if (callbacks >= callbackLimit) { fail('Timer callback limit exceeded'); return; }
      callbacks++;
      try { callback.apply(scope, args); } catch (error) { fail(error); }
    };
    id = (periodic ? interval : timeout)(invoke, delay);
    timers.set(id, {periodic});
    return id;
  }
  scope.setTimeout = (callback, delay = 0, ...args) => schedule(callback, delay, args, false);
  scope.setInterval = (callback, delay = 0, ...args) => schedule(callback, delay, args, true);
  // Browser cancellation APIs share a timer ID space; retain cross-kind cancellation.
  scope.clearTimeout = scope.clearInterval = clear;
  function timerQueue() {
    const entries = [];
    try {
      // fake-timers 15.4.0 owns these normalized deadlines. Do not re-normalize
      // consumer delays or predict recurring deadlines from the requested delay.
      for (const [handle, entry] of timers) {
        if (entries.length === 32) break;
        const id = Number(handle);
        const scheduled = clock.timers?.get(id);
        const dueMs = scheduled?.callAt - epochMs;
        if (!Number.isSafeInteger(id) || id < 1 || !Number.isFinite(dueMs)) {
          throw Error('Timer schedule unavailable');
        }
        entries.push({id, kind:entry.periodic ? 'interval' : 'timeout', dueMs});
      }
      return {timers:entries, omitted:timers.size - entries.length, failure:null};
    } catch {
      // Readback failure must never fault the controlled timeline or run callbacks.
      return {timers:[], omitted:timers.size, failure:'Timer schedule unavailable'};
    }
  }
  function snapshot() {
    return {
      elapsedMs:scope.performance.now(), dateMs:clock.now, performanceMs:scope.performance.now(),
      advancing, failure, pendingTimers:timers.size, frames:frames.snapshot(), cancelledOnFailure,
      timerQueue:timerQueue(),
    };
  }
  async function advance(deltaMs, {frame = true} = {}) {
    if (!Number.isFinite(deltaMs) || deltaMs < 0) throw new RangeError('Invalid time advance');
    if (advancing) throw Error('An advance is already active');
    if (failure) throw Error('Timeline faulted; replace the document');
    const before = snapshot();
    advancing = true;
    callbacks = 0;
    let delivery = null;
    try {
      await clock.tickAsync(deltaMs);
      if (!failure && frame) {
        delivery = await frames.advance(scope.performance.now());
        if (delivery.errors.length) fail(delivery.errors.join('; '));
        // Timers created by the frame at the current instant may now run. Another
        // animation registration still waits for the next explicitly admitted frame.
        if (!failure) await clock.tickAsync(0);
      }
    } catch (error) { fail(error); }
    finally { advancing = false; }
    return {before, requestedMs:before.elapsedMs+deltaMs, after:snapshot(), callbacks, delivery};
  }
  return Object.freeze({snapshot, advance});
}
