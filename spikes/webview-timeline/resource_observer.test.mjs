import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('./resource_observer.js', import.meta.url), 'utf8');
test('resource completion readback drains queued observations without claiming readiness', () => {
  let callback, queue = [], bufferFull;
  const scope = vm.createContext({URL, document:{baseURI:'http://localhost/', readyState:'complete', fonts:{status:'loaded'}},
    performance:{now:() => 50, addEventListener:(name, fn) => { bufferFull = fn; }},
    PerformanceObserver:class {
      constructor(fn) { callback = fn; }
      observe(options) { assert.deepEqual({...options}, {type:'resource', buffered:true}); }
      takeRecords() { const entries = queue; queue = []; return entries; }
    }});
  vm.runInContext(source, scope);
  // Timeline replacement deliberately lacks native Performance event methods.
  scope.performance = {now:() => 50};
  const entry = (n) => ({name:`http://localhost/asset-${n}?secret=not-recorded`, initiatorType:'img', startTime:n, duration:3});
  callback({getEntries:() => [entry(1),entry(2)]});
  queue = [entry(3)];
  bufferFull();
  const read = () => JSON.parse(vm.runInContext('JSON.stringify(__frontcapResources.snapshot())', scope));
  const result = read();
  assert.deepEqual(read(), result);
  assert.equal(result.received, 3);
  assert.equal(result.counts.img, 3);
  assert.equal(result.dropped, 1);
  assert.equal(result.bufferFull, 1);
  assert.equal(result.pendingResources, null);
  assert.equal(result.fontsStatus, 'loaded');
  assert.equal(result.documentState, 'complete');
  assert.deepEqual(result.latest.map(e => e.path), ['/asset-2','/asset-3']);
  assert.equal(result.latest[1].observedElapsedMs, 50);
  assert.equal(result.latest[1].resourceStartMs, 3);
  result.latest[1].path = 'changed';
  assert.equal(read().latest[1].path, '/asset-3');
});

test('unsupported resource timing is explicit', () => {
  const scope = vm.createContext({document:{readyState:'loading'},
    performance:{now:() => 0, addEventListener:() => {}}});
  vm.runInContext(source, scope);
  const result = JSON.parse(vm.runInContext('JSON.stringify(__frontcapResources.snapshot())', scope));
  assert.equal(result.supported, false);
  assert.equal(result.pendingResources, null);
  assert.equal(result.fontsStatus, null);
});
