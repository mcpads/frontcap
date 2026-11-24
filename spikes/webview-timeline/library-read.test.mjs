import {readFile} from 'node:fs/promises';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
const script = await readFile(new URL('../../crates/adapter-native/src/android_page_read.js', import.meta.url), 'utf8');

test('library read retains document ownership without executing page accessors or promising readiness', () => {
  let documentId = 'abcd0000-0000-0000-0000-000000000001';
  const context = vm.createContext({location:{href:'https://fixture.invalid/'},
    document:{readyState:'loading',visibilityState:'visible',hasFocus:() => true},
    performance:{now:() => 0},
    __frontcapFrames:{receipt:() => ({document:documentId}),snapshot:() => ({pending:0})}});
  const read = () => JSON.parse(vm.runInContext(script, context));
  assert.deepEqual(read().canvas_kit, {document:documentId,published:false});
  for (const value of [undefined,null,false,'ready',() => {}]) {
    context.flutterCanvasKit = value;
    assert.equal(read().canvas_kit.published, false);
  }
  context.flutterCanvasKit = {};
  assert.equal(read().canvas_kit.published, true);
  assert.equal(read().ready_state, 'loading');
  documentId = 'abcd0000-0000-0000-0000-000000000002';
  assert.equal(read().canvas_kit.document, documentId);
  Object.defineProperty(context, 'flutterCanvasKit', {get(){throw Error('must not execute');}, configurable:true});
  assert.equal(read().canvas_kit.published, false);
  delete context.__frontcapFrames;
  assert.equal(read().canvas_kit, null);
});
