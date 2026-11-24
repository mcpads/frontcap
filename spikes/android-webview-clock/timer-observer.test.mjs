import test from 'node:test';
import assert from 'node:assert/strict';
import {observeTimers} from './timer-observer.js';

const documentId = 'abcd0000-0000-0000-0000-000000000001';
// A deterministic host double: tests delegation and bookkeeping, not engine scheduling.
function host() {
  const pending = new Map(), calls = [];
  let nextId = 1, time = 10, clockFailure = false;
  const scope = {performance:{now() {
    if (clockFailure) throw Error('clock unavailable');
    return time;
  }}};
  for (const [method, interval] of [['setTimeout', false], ['setInterval', true]]) {
    scope[method] = function (...args) {
      if (this !== scope) throw Error('invalid receiver');
      calls.push({method, args});
      Number(args[1]); // Host argument conversion may invoke consumer code exactly once.
      if (args[0] === 'CSP rejected') return 0;
      const id = nextId++;
      pending.set(id, {callback:args[0], values:args.slice(2), interval});
      return id;
    };
  }
  for (const method of ['clearTimeout', 'clearInterval']) {
    scope[method] = function (id) {
      if (this !== scope) throw Error('invalid receiver');
      calls.push({method, id});
      pending.delete(Number(id) | 0);
      return 'native cancellation result';
    };
  }
  const activity = observeTimers(scope, documentId);
  return {scope, activity, calls, pending,
    reuse(id) {nextId = id;}, failClock() {clockFailure = true;},
    fire(id) {
      const timer = pending.get(id);
      if (!timer.interval) pending.delete(id);
      time++;
      return Reflect.apply(timer.callback, scope, timer.values);
    }};
}

test('native timer delegation preserves arguments, receiver, return values and exceptions', () => {
  const h = host();
  let conversions = 0;
  const delay = {valueOf() {conversions++; return 17;}}, argument = {consumer:'value'};
  const id = h.scope.setTimeout(function (value, number) {
    assert.equal(this, h.scope);assert.equal(value, argument);assert.equal(number, 4);
    const during = h.activity.snapshot();
    assert.equal(during.tracked, 0);assert.equal(during.invoked, 1);assert.equal(during.executing, 1);
    return 'consumer result';
  }, delay, argument, 4);
  assert.equal(conversions, 1);assert.equal(h.calls[0].args[1], delay);
  assert.equal(h.activity.snapshot().timers[0].requestedDelayMs, null);
  const before = h.activity.snapshot();
  assert.deepEqual(h.activity.snapshot(), before);assert.equal(h.calls.length, 1);
  assert.equal(h.fire(id), 'consumer result');assert.equal(h.activity.snapshot().executing, 0);
  const failure = Error('consumer exception');
  const throwing = h.scope.setTimeout(() => {throw failure;}, -10);
  assert.equal(h.activity.snapshot().timers[0].requestedDelayMs, -10);
  assert.throws(() => h.fire(throwing), error => error === failure);
  assert.equal(h.activity.snapshot().executing, 0);
  assert.throws(() => h.scope.setTimeout(() => {}, {valueOf() {throw failure;}}), error => error === failure);
  assert.throws(() => Reflect.apply(h.scope.setInterval, {}, [() => {}, 1]), /invalid receiver/);
  assert.throws(() => h.scope.clearTimeout({valueOf() {throw failure;}}), error => error === failure);
  assert.equal(h.activity.snapshot().registered, 2);
});

test('periodic callbacks, cross-kind cancellation, opaque IDs and ID reuse retain honest evidence', () => {
  const h = host();
  let calls = 0;
  const interval = h.scope.setInterval(() => {
    calls++;
    if (calls === 2) h.scope.clearTimeout(String(interval));
  }, 20);
  h.fire(interval);
  assert.equal(h.activity.snapshot().timers[0].calls, 1);
  h.fire(interval);
  assert.equal(h.activity.snapshot().tracked, 0);
  assert.equal(h.activity.snapshot().canceled, 1);
  const opaque = h.scope.setTimeout(() => {}, 30);
  const remaining = h.scope.setInterval(() => {}, 40);
  let conversions = 0;
  assert.equal(h.scope.clearInterval({valueOf() {conversions++;return opaque;}}), 'native cancellation result');
  assert.equal(conversions, 1);assert.ok(!h.pending.has(opaque));
  assert.equal(h.activity.snapshot().uncertainCancellations, 1);
  h.reuse(opaque);
  h.scope.setTimeout(() => {
    h.reuse(opaque);
    h.scope.setTimeout(() => {}, 50);
  }, 10);
  assert.deepEqual(h.activity.snapshot().timers.map(t => t.id), [remaining, opaque]);
  h.fire(opaque);
  const value = h.activity.snapshot();
  assert.deepEqual(value.timers.map(t => t.registration), [3, 5]);
  h.scope.clearInterval(opaque + 2 ** 32);
  assert.equal(h.activity.snapshot().tracked, 1);
});

test('untracked handlers, bounded rows, replacement and failed observation never become idle claims', () => {
  const h = host();
  assert.equal(h.scope.setTimeout('CSP rejected', 1), 0);
  h.scope.setTimeout('ordinary string handler', 1);
  assert.equal(h.calls[1].args[0], 'ordinary string handler');
  for (let i = 0; i < 4100; i++) h.scope.setTimeout(() => {}, 2);
  const value = h.activity.snapshot();
  assert.equal(value.registered, 4101);assert.equal(value.untrackedRegistrations, 5);
  assert.equal(value.tracked, 4096);assert.equal(value.timers.length, 32);assert.equal(value.omitted, 4064);
  value.timers[0].calls = 900;
  assert.equal(h.activity.snapshot().timers[0].calls, 0);
  h.scope.setTimeout = () => {};
  assert.equal(h.activity.snapshot().methodsObserved, false);
  const broken = host();
  broken.failClock();
  let ran = false;
  const id = broken.scope.setTimeout(() => {
    ran = true;
    assert.equal(broken.activity.snapshot().executing, 1);
    assert.equal(broken.activity.snapshot().invoked, 1);
  }, 1);
  broken.fire(id);assert.ok(ran);
  const unavailable = broken.activity.snapshot();
  assert.equal(unavailable.failure, 'Timer observation failed');assert.equal(unavailable.sampledMs, null);
  assert.equal(unavailable.executing, 0);
});
