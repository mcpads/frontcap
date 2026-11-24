import test from 'node:test';
import assert from 'node:assert/strict';
import {observeConsole} from './console.js';
const document = 'abcd0000-0000-0000-0000-000000000001';
function host() {
  const calls = [], result = {}, thrown = Error('original');
  const console = Object.fromEntries(['log','debug','info','warn','error'].map(name => [name,
    function (...args) { calls.push({receiver:this,args}); if (name === 'error') throw thrown; return result; }]));
  const scope = {console,performance:{now:()=>12}};
  return {console,calls,result,thrown,observer:observeConsole(scope,document)};
}
test('preserves original calls, return and throw without coercing consumer objects', () => {
  const h = host(), receiver = {}, object = new Proxy({}, {get(){throw Error('observer touched object');}});
  assert.equal(h.console.log.call(receiver, 'entity', object, 5), h.result);
  assert.equal(h.calls[0].receiver, receiver);
  assert.equal(h.calls[0].args[1], object);
  assert.throws(()=>h.console.error('failure'), error=>error === h.thrown);
  const s = h.observer.snapshot();
  assert.equal(s.failure, null);
  assert.equal(s.entries[0].text, 'entity [object] 5');
  assert.equal(s.entries[1].text, 'failure');
  assert.equal(s.document, document);
});
test('bounds history and Unicode and reports lost observation after replacement', () => {
  const h = host();
  for (let i=0;i<35;i++) h.console.log('x'.repeat(1023)+'😀');
  const s=h.observer.snapshot();
  assert.equal(s.received,35);assert.equal(s.dropped,3);assert.equal(s.entries.length,32);
  assert.equal(s.entries[0].sequence,4);assert.equal(s.entries[0].text.length,1023);
  assert.equal(s.entries[0].truncated,true);
  s.entries[0].text='changed';assert.notEqual(h.observer.snapshot().entries[0].text,'changed');
  h.console.log=()=>{};assert.match(h.observer.snapshot().failure,/replaced/);
});
test('unavailable console and observer failures never suppress original behavior', () => {
  const console={};assert.match(observeConsole({console},document).snapshot().failure,/unavailable/);
  const h=host();h.console.log('\ud800');
  assert.match(h.observer.snapshot().failure,/failed/);
  assert.equal(h.console.info('still calls original'),h.result);
  assert.equal(h.calls.length,2);
});
