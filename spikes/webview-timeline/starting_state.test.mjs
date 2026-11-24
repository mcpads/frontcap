import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync, writeFileSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
import {randomUUID} from 'node:crypto';
import vm from 'node:vm';

test('same seed reconstructs app randomness without reusing document identity; escaped workers are visible', () => {
  const root = mkdtempSync(join(tmpdir(), 'frontcap-start-'));
  try {
    const timeline = join(root, 'timeline.js');
    writeFileSync(timeline, 'globalThis.__frontcapDocument = crypto.randomUUID();');
    const builder = fileURLToPath(new URL('./prepare_start.py', import.meta.url));
    function build(seed, name) {
      const output = join(root, name);
      execFileSync('python3', [builder, '--timeline', timeline, '--seed', String(seed), '--output', output]);
      return readFileSync(join(output, 'start.js'), 'utf8');
    }
    function child(script) {
      const scope = vm.createContext({EventTarget, CustomEvent, freshIdentity:randomUUID});
      vm.runInContext(`
        class Crypto { randomUUID() { return freshIdentity(); } }
        globalThis.crypto = new Crypto();
        globalThis.document = new EventTarget();
        document.baseURI = 'http://127.0.0.1/app';
        globalThis.window = globalThis; window.top = window;
        globalThis.URL = class { constructor(path) { this.href = path; } };
        globalThis.Worker = class Worker {};
      `, scope);
      vm.runInContext(script, scope);
      return vm.runInContext(`JSON.stringify({document:__frontcapDocument,
        values:[Math.random(),Math.random(),crypto.randomUUID()],
        worker:new Worker('worker.js'), state:__frontcapStartingState.snapshot()})`, scope);
    }
    const script = build(42, 'first');
    const a = JSON.parse(child(script)), b = JSON.parse(child(script));
    assert.notEqual(a.document, b.document);
    assert.deepEqual(a.values, b.values);
    assert.notDeepEqual(a.values, JSON.parse(child(build(43, 'other'))).values);
    assert.deepEqual(a.state.taints, ['worker worker.js']);
    assert.equal(a.state.secureRandomness, false);
    assert.throws(() => build(42, 'first')); // Frozen output cannot be overwritten.
    assert.throws(() => build(-1, 'invalid'));
  } finally { rmSync(root, {recursive:true, force:true}); }
});

test('bounded random diagnostics preserve values, errors and read-only snapshots', () => {
  const observer = readFileSync(new URL('./random_observer.js', import.meta.url), 'utf8');
  function run(trace) {
    const scope = vm.createContext({performance:{now:() => 123}});
    vm.runInContext(`
      let draws = 0;
      Math.random = () => ++draws / 1000;
      class Crypto {
        getRandomValues(view) {
          if (!(view instanceof Uint8Array)) throw new TypeError('integer view required');
          view.fill(7); return view;
        }
        randomUUID() { return 'same-uuid'; }
      }
      globalThis.crypto = new Crypto();
    `, scope);
    if (trace) vm.runInContext(observer, scope);
    const values = vm.runInContext(`JSON.stringify({
      math:Array.from({length:100}, () => Math.random()),
      bytes:Array.from(crypto.getRandomValues(new Uint8Array(4))), uuid:crypto.randomUUID(),
      error:(() => { try { crypto.getRandomValues(null); } catch (e) { return e.message; } })()
    })`, scope);
    const read = () => JSON.parse(vm.runInContext('JSON.stringify(__frontcapRandomCalls.snapshot())', scope));
    return {values, read};
  }
  const baseline = run(false), traced = run(true);
  assert.equal(traced.values, baseline.values);
  const first = traced.read();
  assert.deepEqual(first, traced.read());
  assert.deepEqual(first.counts, {math:100, bytes:1, uuid:1});
  assert.equal(first.received, 102);
  assert.equal(first.first.length, 4);
  assert.equal(first.tail.length, 4);
  assert.equal(first.dropped, 94);
  assert.equal(first.tail.at(-1).sequence, 102);
  assert.equal(first.errors, 0);
  assert.ok(Buffer.byteLength(JSON.stringify(first)) < 3000);
  first.first[0].value = -1;
  assert.notEqual(traced.read().first[0].value, -1);
});

test('iOS typed read exposes document-bound random counts without consuming the stream', () => {
  const root=mkdtempSync(join(tmpdir(),'frontcap-random-read-'));
  try {
    const timeline=join(root,'timeline.js');
    writeFileSync(timeline,'globalThis.__frontcapDocument = crypto.randomUUID();');
    const builder=fileURLToPath(new URL('./prepare_start.py',import.meta.url));
    const control=readFileSync(new URL('./control.m',import.meta.url),'utf8');
    const field=control.match(/"(starting_randomness:[^"\n]+)"/)[1];
    function child(trace,name) {
      const output=join(root,name);
      execFileSync('python3',[builder,'--timeline',timeline,'--seed','42','--output',output,...(trace?['--trace-random']:[])]);
      const scope=vm.createContext({EventTarget,CustomEvent,freshIdentity:randomUUID,performance:{now:()=>0}});
      vm.runInContext(`class Crypto {randomUUID(){return freshIdentity();}}
        globalThis.crypto=new Crypto(); globalThis.document=new EventTarget();
        document.baseURI='http://fixture.invalid/'; globalThis.window=globalThis;window.top=window;
        globalThis.URL=class {constructor(path){this.href=path;}};globalThis.Worker=class {};`,scope);
      vm.runInContext(readFileSync(join(output,'start.js'),'utf8'),scope);
      const read=()=>JSON.parse(vm.runInContext(`JSON.stringify({${field}})`,scope)).starting_randomness;
      return {scope,read};
    }
    const a=child(true,'a'),b=child(true,'b'),untraced=child(false,'untraced');
    assert.equal(untraced.read(),null);
    assert.equal(a.read().math_calls,0);
    const library=()=>JSON.parse(vm.runInContext('JSON.stringify(__frontcapStartingState.snapshot().canvasKit)',a.scope));
    assert.deepEqual(library(),{document:a.read().document,published:false});
    a.scope.flutterCanvasKit={};
    assert.deepEqual(library(),{document:a.read().document,published:true});
    Object.defineProperty(a.scope,'flutterCanvasKit',{get(){throw Error('must not execute');},configurable:true});
    assert.equal(library().published,false);
    assert.notEqual(a.read().document,b.read().document);
    const draw='JSON.stringify([Math.random(),...crypto.getRandomValues(new Uint8Array(4)),crypto.randomUUID()])';
    assert.equal(vm.runInContext(draw,a.scope),vm.runInContext(draw,b.scope));
    vm.runInContext("new Worker('worker.js')",a.scope);
    const before=a.read();
    assert.deepEqual(before,a.read());
    assert.equal(before.document,vm.runInContext('__frontcapDocument',a.scope));
    assert.equal(before.seed,42);assert.equal(before.math_calls,1);
    assert.equal(before.crypto_byte_calls,1);assert.equal(before.crypto_uuid_calls,1);
    assert.equal(before.uncontrolled_calls,1);assert.equal(before.observation_errors,0);
    assert.equal(vm.runInContext('Math.random()',a.scope),vm.runInContext('Math.random()',b.scope));
  } finally {rmSync(root,{recursive:true,force:true});}
});

test('random caller window is bounded, half-open and snapshots cannot alter counts', () => {
  const source=readFileSync(new URL('./random_observer.js',import.meta.url),'utf8').replace('const callWindow = null;','const callWindow = [1000,1500];');
  let elapsed=999;
  const scope=vm.createContext({performance:{now:()=>elapsed}});
  vm.runInContext(source,scope);
  vm.runInContext('Math.random()',scope);
  elapsed=1000;
  for(let i=0;i<20;i++) vm.runInContext(`function caller${i}(){return Math.random()} caller${i}()`,scope);
  elapsed=1500;vm.runInContext('Math.random()',scope);
  const read=()=>JSON.parse(vm.runInContext('JSON.stringify(__frontcapRandomCalls.snapshot())',scope));
  const before=read();
  assert.equal(before.counts.math,22);
  assert.equal(before.callWindow.calls,20);
  assert.equal(before.callWindow.callers.length,16);
  assert.equal(before.callWindow.omittedCalls,4);
  vm.runInContext('__frontcapRandomCalls.snapshot().callWindow.callers[0].calls=999',scope);
  assert.deepEqual(read(),before);
});
