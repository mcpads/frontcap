import test from 'node:test';
import assert from 'node:assert/strict';
import {observePageEvents} from './page-events.js';

function host() {
  const listeners = new Map();
  const scope = {performance:{now:() => 500},
    addEventListener(kind, callback) {listeners.set(kind, callback);},
    document:{visibilityState:'visible', addEventListener(kind, callback) {listeners.set(kind, callback);}}};
  return {scope, observer:observePageEvents(scope),
    message(data, origin='http://localhost') {listeners.get('message')({data, origin});},
    visibility() {listeners.get('visibilitychange')();}};
}

test('records actual delivery without interpreting objects or doing work on reads', () => {
  const h = host();
  const empty = h.observer.snapshot();
  assert.deepEqual(empty, {received:0,dropped:0,events:[]});
  h.message('{"id":1,"domain":"CO"}');
  h.message({get secret() {throw Error('Consumer getter must not run');}, toString() {throw Error('No coercion');}});
  h.scope.document.visibilityState='hidden';h.visibility();
  const value=h.observer.snapshot();
  assert.equal(value.events[0].data,'{"id":1,"domain":"CO"}');
  assert.equal(value.events[1].dataType,'object');assert.equal(value.events[1].data,null);
  assert.equal(value.events[2].state,'hidden');
  assert.ok(value.events.every(e => e.elapsedMs === 500));
  h.scope.performance.now=()=>{throw Error('Snapshot must not sample or advance');};
  assert.deepEqual(h.observer.snapshot(),value);
  value.events[0].data='mutated copy';assert.notEqual(h.observer.snapshot().events[0].data,'mutated copy');
  assert.deepEqual(host().observer.snapshot(),empty);
});

test('bounded tails preserve loss and Unicode boundaries', () => {
  const h=host();
  for(let i=0;i<12;i++) h.message('x'.repeat(63)+'🚆', 'x'.repeat(31)+'🚆');
  const value=h.observer.snapshot();
  assert.equal(value.received,12);assert.equal(value.dropped,4);
  assert.deepEqual(value.events.map(e=>e.sequence),[5,6,7,8,9,10,11,12]);
  assert.ok(value.events.every(e=>e.data==='x'.repeat(63)&&e.truncated&&e.origin==='x'.repeat(31)&&e.originTruncated));
});

test('observer failure preserves known history and cannot masquerade as complete coverage', () => {
  const h=host();h.message('before');
  assert.doesNotThrow(()=>h.message('\ud800'));
  h.message('after');
  const value=h.observer.snapshot();
  assert.equal(value.received,1);assert.equal(value.events[0].data,'before');
  assert.match(value.failure,/subsequent events are not covered/);
});
