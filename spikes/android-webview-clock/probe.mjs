// Opt-in capability measurement against an owned Android WebView debugger.
// This does not qualify frontcap's patched-engine guarantees or app coverage.
import {readFile, writeFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
const [endpoint, output, frameGate] = process.argv.slice(2);
if (!endpoint || !output) throw Error('usage: node probe.mjs http://127.0.0.1:PORT OUTPUT.json [FRAME_GATE.js]');
const base = new URL(endpoint);
if (base.hostname !== '127.0.0.1') throw Error('Only an owned loopback debugger is accepted');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const report = {endpoint, started: new Date().toISOString(), commands: [], observations: {}};
let ws;
let sequence = 0;
const pending = new Map();
const events = [];
try {
  report.version = await (await fetch(new URL('/json/version', base))).json();
  const pages = await (await fetch(new URL('/json/list', base))).json();
  const targets = pages.filter(page => page.type === 'page');
  if (targets.length !== 1) throw Error(`Expected one owned WebView page, got ${targets.length}`);
  report.target = targets[0];
  const socketUrl = new URL(targets[0].webSocketDebuggerUrl);
  socketUrl.host = base.host;
  ws = new WebSocket(socketUrl);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(Error('WebSocket open deadline')), 5000);
    ws.onopen = () => { clearTimeout(timer); resolve(); };
    ws.onerror = () => { clearTimeout(timer); reject(Error('WebSocket open failed')); };
  });
  ws.onmessage = event => {
    const message = JSON.parse(event.data);
    if (!message.id) { events.push(message); return; }
    const waiter = pending.get(message.id);
    if (waiter) { pending.delete(message.id); clearTimeout(waiter.timer); waiter.resolve(message); }
  };
  async function send(method, params = {}) {
    const id = ++sequence;
    const reply = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(Error(`${method} deadline`)); }, 8000);
      pending.set(id, {resolve, timer});
      ws.send(JSON.stringify({id, method, params}));
    });
    report.commands.push({method, params, reply});
    if (reply.error) throw Error(`${method}: ${JSON.stringify(reply.error)}`);
    return reply.result;
  }
  async function read() {
    const result = await send('Runtime.evaluate', {
      expression: 'JSON.stringify({...window.clockProbe, date:Date.now(), performance:performance.now()})',
      returnByValue: true,
    });
    if (result.exceptionDetails) throw Error(JSON.stringify(result.exceptionDetails));
    return JSON.parse(result.result.value);
  }
  await send('Page.enable');
  await send('Runtime.enable');
  if (frameGate) {
    const source = await readFile(frameGate, 'utf8');
    report.frameGateSha256 = createHash('sha256').update(source).digest('hex');
    await send('Page.addScriptToEvaluateOnNewDocument', {source});
  }
  // Load the probe document in the disposable test host.
  await send('Page.navigate', {url: 'about:blank'});
  await sleep(250);
  await send('Runtime.evaluate', {expression: `
    window.clockProbe = {timers:0, frames:0, frameTimestamp:null};
    setInterval(() => clockProbe.timers++, 10);
    function frame(t) { clockProbe.frames++; clockProbe.frameTimestamp=t; requestAnimationFrame(frame); }
    requestAnimationFrame(frame);
  `});
  report.observations.runningBefore = await read();
  await sleep(150);
  report.observations.runningAfter = await read();
  await send('Emulation.setVirtualTimePolicy', {policy:'pause'});
  report.observations.heldBefore = await read();
  await sleep(250);
  report.observations.heldAfter = await read();
  const eventStart = events.length;
  await send('Emulation.setVirtualTimePolicy', {policy:'advance', budget:100});
  const deadline = Date.now()+5000;
  while (!events.slice(eventStart).some(event => event.method === 'Emulation.virtualTimeBudgetExpired')) {
    if (Date.now() > deadline) throw Error('Virtual budget expiry not observed');
    await sleep(10);
  }
  if (frameGate) {
    const delivery = await send('Runtime.evaluate', {
      expression: '__frontcapFrames.advance(performance.now())',
      awaitPromise: true, returnByValue: true,
    });
    if (delivery.exceptionDetails) throw Error(JSON.stringify(delivery.exceptionDetails));
    report.frameDelivery = delivery.result.value;
  }
  report.observations.advanced = await read();
  await sleep(250);
  report.observations.afterBudget = await read();
  for (const [name, params] of [
    ['Emulation.getVirtualTime', {}], ['HeadlessExperimental.beginFrame', {frameTimeTicks:100}],
  ]) {
    try { await send(name, params); } catch (error) { report.commands.at(-1).capabilityError = String(error); }
  }
  const o = report.observations;
  report.measurement = {
    runningTimerDelta:o.runningAfter.timers-o.runningBefore.timers,
    heldTimerDelta:o.heldAfter.timers-o.heldBefore.timers,
    heldFrameDelta:o.heldAfter.frames-o.heldBefore.frames,
    heldDateDelta:o.heldAfter.date-o.heldBefore.date,
    heldPerformanceDelta:o.heldAfter.performance-o.heldBefore.performance,
    advancedTimerDelta:o.advanced.timers-o.heldAfter.timers,
    advancedFrameDelta:o.advanced.frames-o.heldAfter.frames,
    advancedDateDelta:o.advanced.date-o.heldAfter.date,
    advancedPerformanceDelta:o.advanced.performance-o.heldAfter.performance,
    afterBudgetTimerDelta:o.afterBudget.timers-o.advanced.timers,
    afterBudgetFrameDelta:o.afterBudget.frames-o.advanced.frames,
  };
  report.completed = true;
} catch (error) {
  report.error = String(error);
  process.exitCode = 1;
} finally {
  report.events = events;
  report.finished = new Date().toISOString();
  for (const waiter of pending.values()) clearTimeout(waiter.timer);
  if (ws) ws.close();
  await writeFile(output, JSON.stringify(report, null, 2)+'\n');
}
