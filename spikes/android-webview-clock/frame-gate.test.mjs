import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {webcrypto} from 'node:crypto';
import {readFile} from 'node:fs/promises';
const source = await readFile(new URL('./frame-gate.js', import.meta.url), 'utf8');
function host(failAt = null) {
  let sequence = 0;
  const pending = new Map();
  const errors = [];
  const context = vm.createContext({
    crypto:webcrypto,
    requestAnimationFrame(callback) { const id = ++sequence; if (id === failAt) throw Error("native scheduling failed"); pending.set(id, callback); return id; },
    cancelAnimationFrame(id) { pending.delete(id); },
  });
  vm.runInContext(source, context);
  return {context, errors, async frame() {
    for (const id of [...pending.keys()]) {
      const callback = pending.get(id);
      if (!callback) continue;
      pending.delete(id);
      try { callback(9000); } catch (error) { errors.push(error); }
      // Native callback delivery performs a microtask checkpoint before the next callback.
      await new Promise(resolve => setImmediate(resolve));
    }
  }};
}

test('holds registrations; cancellation and nested requests cross the correct frame boundary', async () => {
  const h = host();
  vm.runInContext(`
    var calls=[];
    requestAnimationFrame(t => {
      calls.push(['first',t]);
      requestAnimationFrame(t => calls.push(['nested',t]));
      Promise.resolve().then(() => Promise.resolve().then(() => cancelAnimationFrame(second)));
    });
    var second=requestAnimationFrame(t => calls.push(['cancelled',t]));
  `, h.context);
  const sampled = h.context.__frontcapFrames.snapshot();
  assert.equal(sampled.pending, 2);
  assert.equal(sampled.delivered, 0);
  assert.equal(sampled.dispatching, false);
  assert.deepEqual(h.context.__frontcapFrames.snapshot(), sampled);
  sampled.pending = 0;
  assert.equal(h.context.__frontcapFrames.snapshot().pending, 2);
  await h.frame();
  assert.equal(h.context.calls.length, 0);
  const first = h.context.__frontcapFrames.advance(10);
  await h.frame();
  assert.equal((await first).delivered, 1);
  assert.equal(h.context.__frontcapFrames.snapshot().pending, 1);
  const second = h.context.__frontcapFrames.advance(20);
  await h.frame();
  assert.equal((await second).delivered, 1);
  assert.equal(JSON.stringify(h.context.calls), '[["first",10],["nested",20]]');
});

test('rejects overlapping delivery and backward/nonfinite timestamps without dispatch', async () => {
  const h = host();
  const first = h.context.__frontcapFrames.advance(10);
  assert.throws(() => h.context.__frontcapFrames.advance(20), /already pending/);
  await h.frame();
  await first;
  for (const time of [9, -1, Infinity, NaN]) {
    assert.throws(() => h.context.__frontcapFrames.advance(time), /monotonic/);
  }
  assert.equal(h.context.__frontcapFrames.snapshot().lastTimestamp, 10);
});

test('uncaught callback error is retained and does not swallow subsequent callbacks', async () => {
  const h = host();
  vm.runInContext(`
    requestAnimationFrame(() => { throw Error('app failure'); });
    var reached=false;
    requestAnimationFrame(() => { reached=true; });
  `, h.context);
  const result = h.context.__frontcapFrames.advance(10);
  await h.frame();
  assert.equal((await result).errors.length, 1);
  assert.equal(h.errors.length, 1);
  assert.equal(h.context.reached, true);
  assert.equal(h.context.__frontcapFrames.snapshot().dispatching, false);
});


test('frame receipts preserve pending and completed outcomes without repeating callbacks', async () => {
  const h=host();const gate=h.context.__frontcapFrames;
  vm.runInContext('var delivered=0;requestAnimationFrame(() => delivered++);',h.context);
  const document=gate.receipt().document;
  const command={document,id:1,timestamp:10};
  assert.equal(gate.dispatch(command).last.state,'pending');
  assert.equal(gate.dispatch(command).last.state,'pending');
  assert.throws(() => gate.dispatch({...command,id:2}),/unresolved/);
  assert.throws(() => gate.advance(20),/receipt-bound/);
  await h.frame();
  const completed=gate.receipt();
  assert.equal(completed.last.state,'completed');
  assert.equal(completed.last.result.delivered,1);
  assert.equal(h.context.delivered,1);
  assert.equal(JSON.stringify(gate.dispatch(command)),JSON.stringify(completed));
  // A returned object cannot corrupt the producer's retained outcome.
  completed.last.result.delivered=999;
  assert.equal(gate.receipt().last.result.delivered,1);
  assert.throws(() => gate.dispatch({...command,timestamp:11}),/conflict/);
  assert.throws(() => gate.dispatch({...command,id:3}),/not next/);
  assert.throws(() => gate.dispatch({...command,id:2,timestamp:9}),/monotonic/);
  assert.equal(gate.dispatch({...command,id:2,timestamp:20}).last.state,'pending');
  await h.frame();
  assert.throws(() => gate.dispatch(command),/not next/);
  const other=host();
  assert.notEqual(other.context.__frontcapFrames.receipt().document,document);
  assert.throws(() => other.context.__frontcapFrames.dispatch(command),/identity/);
});

test('legacy advancement cannot be mixed with receipt-bound control', async () => {
  const h=host();const gate=h.context.__frontcapFrames;
  const pending=gate.advance(1);await h.frame();await pending;
  assert.equal(gate.receipt().legacyUsed,true);
  assert.throws(() => gate.dispatch({document:gate.receipt().document,id:1,timestamp:2}),/without request receipts/);
});


test('partial native scheduling failure stays unresolved and cannot be replayed', async () => {
  const h=host(2);const gate=h.context.__frontcapFrames;
  vm.runInContext('var delivered=0;requestAnimationFrame(() => delivered++);',h.context);
  const command={document:gate.receipt().document,id:1,timestamp:10};
  assert.equal(gate.dispatch(command).last.state,'pending');
  await h.frame();
  assert.equal(h.context.delivered,1);
  assert.equal(gate.dispatch(command).last.state,'unresolved');
  assert.throws(() => gate.dispatch({...command,id:2,timestamp:20}),/unresolved/);
  await h.frame();assert.equal(h.context.delivered,1);
});
