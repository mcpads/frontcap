// Main world, in every frame, before any page script: callbacks that native rendering would drive are run
// from virtual-time timers on frame ticks (multiples of the frame interval from the time origin). While
// virtual time is paused no timer runs, so none of these callbacks run either, and how many run depends on
// virtual time only. On the patched engine, which renders main frames at frame ticks only (engine patch 6),
// animation frames and resize and intersection observations are the engine's own, with the browser's
// semantics (a microtask checkpoint after each callback); only idle callbacks are run here.
(() => {
  const ORIGIN = __FRONTCAP_INITIAL_DATE_MS__;
  const FRAME_MS = __FRONTCAP_FRAME_MS__;
  const NATIVE_FRAMES = __FRONTCAP_NATIVE_FRAMES__;
  // Taken before any page script, which could replace it.
  const Channel = MessageChannel;
  const now = () => Date.now() - ORIGIN;
  const report = (e) => (typeof reportError === "function" ? reportError(e) : setTimeout(() => { throw e; }, 0));

  // One shared tick: the next frame tick strictly after the last one that ran.
  let lastTick = -1;
  const tickQueue = [];
  let tickScheduled = false;
  function onTick(fn) {
    tickQueue.push(fn);
    if (tickScheduled) return;
    tickScheduled = true;
    const t = now();
    const k = Math.max(Math.floor(t / FRAME_MS) + 1, lastTick + 1);
    // Timers take whole milliseconds; ceil keeps the callback at or after the tick, never before it.
    setTimeout(runTick, Math.max(0, Math.ceil(k * FRAME_MS - t)), k);
  }
  function runTick(k) {
    tickScheduled = false;
    lastTick = k;
    // A rendering opportunity: style and layout are brought up to date before frame callbacks, so a style
    // change made in one frame is observed (and its transitions are created) before the next.
    if (document.documentElement) document.documentElement.getBoundingClientRect();
    const batch = tickQueue.splice(0);
    for (const fn of batch) fn(k * FRAME_MS);
  }

  if (!NATIVE_FRAMES) {
    // requestAnimationFrame: callbacks registered during a frame run on the next frame; a callback cancelled
    // during the current frame's batch does not run.
    let nextId = 1;
    let pending = new Map();
    let frameArmed = false;
    function runFrame() {
      frameArmed = false;
      const batch = pending;
      pending = new Map();
      const ts = performance.now();
      running = batch;
      for (const [id, cb] of batch) {
        if (!batch.has(id)) continue;
        try { cb(ts); } catch (e) { report(e); }
      }
      running = null;
    }
    let running = null;
    window.requestAnimationFrame = function requestAnimationFrame(cb) {
      const id = nextId++;
      pending.set(id, cb);
      if (!frameArmed) { frameArmed = true; onTick(runFrame); }
      return id;
    };
    window.cancelAnimationFrame = function cancelAnimationFrame(id) {
      pending.delete(id);
      if (running) running.delete(id);
    };
  }

  // Idle callbacks run on the next tick with a fixed deadline, so work loops make progress.
  let nextIdle = 1;
  const idle = new Map();
  window.requestIdleCallback = function requestIdleCallback(cb) {
    const id = nextIdle++;
    idle.set(id, cb);
    // Each callback a task of its own at the tick, as the browser's idle tasks are: microtasks one queues
    // run before the next. A message, not a zero timer, which nesting would clamp to 4 ms after the tick.
    onTick((tickTime) => {
      const channel = new Channel();
      channel.port1.onmessage = () => {
        channel.port1.close();
        const fn = idle.get(id);
        if (!fn) return;
        idle.delete(id);
        const deadline = tickTime + FRAME_MS / 2;
        try { fn({ didTimeout: false, timeRemaining: () => Math.max(0, deadline - now()) }); } catch (e) { report(e); }
      };
      channel.port2.postMessage(0);
    });
    return id;
  };
  window.cancelIdleCallback = function cancelIdleCallback(id) { idle.delete(id); };

  if (!NATIVE_FRAMES) {
    // Rendering-driven observers deliver on the next tick instead of from native rendering steps.
    for (const name of ["ResizeObserver", "IntersectionObserver"]) {
      const Native = window[name];
      if (typeof Native !== "function") continue;
      window[name] = class extends Native {
        constructor(callback, options) {
          let queued = [];
          let armed = false;
          super((entries) => {
            queued.push(...entries);
            if (armed) return;
            armed = true;
            onTick(() => {
              armed = false;
              const batch = queued;
              queued = [];
              try { callback(batch, this); } catch (e) { report(e); }
            });
          }, options);
        }
      };
    }
  }
})();
