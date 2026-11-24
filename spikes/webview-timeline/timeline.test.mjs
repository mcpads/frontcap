import test from 'node:test';
import assert from 'node:assert/strict';
import {installTimeline} from './timeline.js';
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
function host(options) {
  const scope = {Date, Promise, setTimeout, clearTimeout, setInterval, clearInterval,
    performance:{now:() => performance.now()}};
  const frames = {snapshot:() => ({pending:0}), advance:async timestamp => ({timestamp,errors:[]})};
  const timeline = installTimeline(scope, frames, options);
  return {scope, frames, timeline};
}

test('holds clocks and timers, then drains deadline callbacks and native promise work', async () => {
  const {scope, timeline} = host();
  const calls = [];
  scope.setTimeout(() => {
    calls.push(['first',scope.Date.now(),scope.performance.now()]);
    Promise.resolve().then(() => {
      calls.push(['promise']);
      scope.setTimeout(() => calls.push(['nested',scope.performance.now()]), 5);
    });
  }, 10);
  const before = timeline.snapshot();
  await sleep(20);
  assert.deepEqual(timeline.snapshot(), before);
  assert.equal(calls.length, 0);
  const result = await timeline.advance(20);
  assert.deepEqual(calls, [['first',1700000000010,10],['promise'],['nested',15]]);
  assert.equal(result.delivery.timestamp, 20);
  assert.equal(result.after.pendingTimers, 0);
  assert.equal(result.after.dateMs, 1700000000020);
});

test('intervals can cancel themselves using the shared timer ID space', async () => {
  const {scope, timeline} = host();
  let calls = 0;
  const id = scope.setInterval(() => { if (++calls === 3) scope.clearTimeout(id); }, 5);
  const removed = scope.setTimeout(() => assert.fail('cancelled timeout ran'), 2);
  scope.clearInterval(removed);
  await timeline.advance(30);
  assert.equal(calls, 3);
  assert.equal(timeline.snapshot().pendingTimers, 0);
});

test('zero-delay runaway work faults and fences the document within the callback bound', async () => {
  const {scope, timeline} = host({callbackLimit:5});
  scope.setInterval(() => {}, 0);
  const result = await timeline.advance(100);
  assert.equal(result.callbacks, 5);
  assert.match(result.after.failure, /callback limit/);
  assert.equal(result.after.pendingTimers, 0);
  assert.equal(result.delivery, null);
  await assert.rejects(timeline.advance(1), /replace the document/);
});

test('overlap and invalid advances reject before applying a second operation', async () => {
  const {timeline} = host();
  const first = timeline.advance(10);
  await assert.rejects(timeline.advance(20), /already active/);
  await first;
  await assert.rejects(timeline.advance(-1), /Invalid/);
  assert.equal(timeline.snapshot().elapsedMs, 10);
});

test('a timer exception faults the domain and cancels pending callbacks', async () => {
  const {scope, timeline} = host();
  scope.setTimeout(() => {throw Error('app callback failed');}, 5);
  scope.setTimeout(() => assert.fail('must not run after fault'), 10);
  const result = await timeline.advance(20);
  assert.match(result.after.failure, /app callback failed/);
  assert.equal(result.after.pendingTimers, 0);
  assert.equal(result.delivery, null);
});


test('fractional advances retain the performance-clock frontier while Date is integral', async () => {
  const {timeline} = host();
  const first = await timeline.advance(.25);
  assert.equal(first.after.elapsedMs, .25);
  const second = await timeline.advance(.75);
  assert.equal(second.requestedMs, 1);
  assert.equal(second.after.elapsedMs, 1);
  assert.equal(second.after.dateMs, 1700000000001);
});

test('pending readback follows normalized deadlines, repetition, cancellation and bounded omission', async () => {
  const {scope, timeline} = host();
  const interval = scope.setInterval(() => {}, 5);
  scope.setTimeout(() => {}, '12');
  await timeline.advance(.25, {frame:false});
  const before = timeline.snapshot();
  assert.deepEqual(before.timerQueue.timers.map(t => [t.kind,t.dueMs]), [['interval',5],['timeout',12]]);
  assert.deepEqual(timeline.snapshot(), before);
  before.timerQueue.timers[0].dueMs = 999;
  assert.equal(timeline.snapshot().timerQueue.timers[0].dueMs, 5);
  await timeline.advance(5, {frame:false});
  assert.deepEqual(timeline.snapshot().timerQueue.timers.map(t => t.dueMs), [10,12]);
  scope.clearTimeout(interval);
  await timeline.advance(7, {frame:false});
  assert.equal(timeline.snapshot().pendingTimers, 0);
  assert.deepEqual(timeline.snapshot().timerQueue, {timers:[],omitted:0,failure:null});
  for (let i=0; i<40; i++) scope.setTimeout(() => {}, 100+i);
  const bounded = timeline.snapshot();
  assert.equal(bounded.pendingTimers, 40);assert.equal(bounded.timerQueue.timers.length, 32);
  assert.equal(bounded.timerQueue.omitted, 8);
  assert.equal(new Set(bounded.timerQueue.timers.map(t => t.id)).size, 32);
});

test('unavailable scheduler metadata remains a readback failure, not a timeline fault', async () => {
  const {scope, timeline} = host();
  let ran = false;
  const id = scope.setTimeout(() => {ran=true;}, 5);
  const scheduled = scope.Date.clock.timers.get(Number(id));
  const due = scheduled.callAt;
  Object.defineProperty(scheduled, 'callAt', {configurable:true, get() {throw Error('unavailable');}});
  const unavailable = timeline.snapshot();
  assert.equal(unavailable.failure, null);assert.equal(unavailable.pendingTimers, 1);
  assert.deepEqual(unavailable.timerQueue, {timers:[],omitted:1,failure:'Timer schedule unavailable'});
  assert.equal(ran, false);
  Object.defineProperty(scheduled, 'callAt', {configurable:true, writable:true, value:due});
  await timeline.advance(5, {frame:false});
  assert.equal(ran, true);assert.equal(timeline.snapshot().failure, null);
});
