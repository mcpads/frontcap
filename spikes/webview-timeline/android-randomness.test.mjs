import {readFileSync} from 'node:fs';
import {randomFillSync} from 'node:crypto';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
const read = path => readFileSync(new URL(path, import.meta.url), 'utf8');
const frame = read('../android-webview-clock/frame-gate.js');
const template = read('../../crates/adapter-native/src/android_starting_randomness.js');
const random = read('../../crates/adapter-chromium/src/js/seed.js');
const observer = read('./random_observer.js');
function child(seed) {
  const context = vm.createContext({EventTarget, CustomEvent, URL, freshBytes:randomFillSync,
    requestAnimationFrame:() => 1, cancelAnimationFrame:() => {}, performance:{now:() => 0}});
  vm.runInContext(`
    class Crypto {getRandomValues(view) {return freshBytes(view);}}
    globalThis.crypto = new Crypto();
    globalThis.document = new EventTarget(); document.baseURI = 'http://fixture.invalid/';
    globalThis.window = globalThis; window.top = window;
    globalThis.Worker = class {};
  `, context);
  vm.runInContext(frame, context);
  const script = template.replace('__FRONTCAP_RANDOM_SOURCE__', random)
    .replace('__FRONTCAP_RANDOM_OBSERVER__', observer)
    .replaceAll('__FRONTCAP_SEED__', String(seed))
    .replaceAll('__FRONTCAP_DIALOG_EVENT__', 'frontcap:starting-randomness');
  vm.runInContext(script, context);
  const evaluate = code => JSON.parse(vm.runInContext(`JSON.stringify(${code})`, context));
  return {context, script, evaluate, state:() => evaluate('__frontcapStartingRandomness.snapshot()')};
}
test('Android seed preserves fresh frame identity, repeatable draws and non-consuming diagnostics', () => {
  const a=child(42), b=child(42), c=child(43);
  const draw = `({math:[Math.random(), Math.random()], bytes:[...crypto.getRandomValues(new Uint8Array(8))], uuid:crypto.randomUUID()})`;
  const first=a.evaluate(draw);
  assert.deepEqual(first,b.evaluate(draw)); assert.notDeepEqual(first,c.evaluate(draw));
  const before=a.state();
  assert.notEqual(before.document,b.state().document);
  assert.equal(before.document,a.evaluate('__frontcapFrames.receipt().document'));
  assert.deepEqual(before,a.state());
  assert.equal(before.seed,42); assert.equal(before.math_calls,2);
  assert.equal(before.crypto_byte_calls,1); assert.equal(before.crypto_uuid_calls,1);
  assert.equal(before.observation_errors,0); assert.equal(before.uncontrolled_calls,0);
  vm.runInContext("new Worker('worker.js')",a.context);
  assert.equal(a.state().uncontrolled_calls,1);
  assert.throws(() => vm.runInContext(a.script,a.context),/already prepared/);
  // Neither observation nor a rejected second installation resets/consumes the original stream.
  assert.equal(a.evaluate('Math.random()'),b.evaluate('Math.random()'));
});
