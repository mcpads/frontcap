import {test} from 'node:test';
import assert from 'node:assert/strict';
import {observePointers} from './pointers.js';
test('passive observations retain contact transitions and a bounded independent event tail', () => {
  const listeners=new Map();
  const scope={performance:{now:()=>123},addEventListener(kind,listener,options){
    assert.deepEqual(options,{capture:true,passive:true});listeners.set(kind,listener);
  }};
  const observer=observePointers(scope);
  const emit=(kind,id)=>listeners.get(kind)({pointerId:id,isTrusted:true,clientX:10,clientY:20});
  emit('pointerdown',1);emit('pointerdown',2);emit('pointerup',2);
  emit('pointermove',1);emit('pointercancel',1);
  const first=observer.snapshot();
  assert.equal(first.maximumActive,2);assert.equal(first.active,0);
  assert.deepEqual(first.events.map(e=>e.active),[1,2,1,1,0]);
  first.events[0].kind='mutated';
  assert.equal(observer.snapshot().events[0].kind,'pointerdown');
  for(let i=0;i<6;i++)emit('pointermove',3);
  const tail=observer.snapshot();
  assert.equal(tail.received,11);assert.equal(tail.dropped,3);
  assert.deepEqual(tail.events.map(e=>e.sequence),[4,5,6,7,8,9,10,11]);
  assert.equal(tail.events.at(-1).pageMs,123);
});
