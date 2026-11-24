onmessage=({data:{canvas,color,synchronous}})=>{
 const context=canvas.getContext('2d');
 const callbacks=[];
 requestAnimationFrame(time=>{
  const first={time,before:Date.now()};
  if(synchronous){
   const request=new XMLHttpRequest();
   request.open('GET','https://api.example.test/frame-service',false);
   request.send();first.body=request.responseText;first.status=request.status;
  }
  first.after=Date.now();callbacks.push(first);
  context.fillStyle=color;context.fillRect(0,0,64,64);
  requestAnimationFrame(time=>{
   callbacks.push({time,before:Date.now(),after:Date.now()});
   context.fillStyle=color;context.fillRect(0,0,64,64);
   postMessage({callbacks});
  });
 });
};
