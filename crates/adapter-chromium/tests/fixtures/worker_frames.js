onmessage=({data:{canvas,color}})=>{
 const context=canvas.getContext('2d');
 const order=[];let flag='unset';let cancelled;let first;
 const discarded=requestAnimationFrame(()=>order.push('direct cancellation failed'));
 cancelAnimationFrame(discarded);
 requestAnimationFrame(time=>{
  first={time,date:Date.now()};order.push('first');
  queueMicrotask(()=>{order.push('microtask');flag='set';cancelAnimationFrame(cancelled)});
 });
 requestAnimationFrame(()=>order.push('second:'+flag));
 cancelled=requestAnimationFrame(()=>order.push('cancelled callback ran'));
 requestAnimationFrame(()=>{
  order.push('fourth');Promise.resolve().then(()=>order.push('promise'));
  requestAnimationFrame(time=>{
   order.push('next');context.fillStyle=color;context.fillRect(0,0,64,64);
   postMessage({order,first,next:{time,date:Date.now()}});
  });
 });
};
