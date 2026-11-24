import {test} from 'node:test';
import assert from 'node:assert/strict';
import {StdoutStream} from './stream.mjs';
const event=(bytes,id='isolates/1')=>({method:'streamNotify',params:{streamId:'Stdout',event:{kind:'WriteEvent',bytes:Buffer.from(bytes).toString('base64'),isolate:{id}}}});
test('retains pre-ack events and reconstructs split Unicode separately for each isolate',()=>{
  const s=new StdoutStream();const raw=Buffer.from('결과\n');
  s.accept(event(raw.subarray(0,2)));s.accept(event('other','isolates/2'));
  s.accept({id:1,result:{type:'Success'}});s.accept(event(raw.subarray(2)));
  assert.deepEqual(s.snapshot().streams,[{isolate:'isolates/1',text:'결과\n'},{isolate:'isolates/2',text:'other'}]);
  assert.equal(s.snapshot().subscribed,true);
});
test('capacity exhaustion retains prior evidence and does not claim a complete stream',()=>{
  const s=new StdoutStream({maxBytes:4,maxEvents:2});s.accept(event('1234'));
  assert.throws(()=>s.accept(event('5')),/capacity/);assert.equal(s.snapshot().bytes,4);
});
test('rejected subscription and malformed bytes are explicit failures',()=>{
  const s=new StdoutStream();assert.throws(()=>s.accept({id:1,error:{code:103}}),/subscription/);
  const bad=event('a');bad.params.event.bytes='???';assert.throws(()=>s.accept(bad),/base64/);
  assert.equal(s.snapshot().subscribed,false);
});
