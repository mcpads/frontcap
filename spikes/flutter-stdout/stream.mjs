// Bounded VM-service stdout observation. Never evaluates consumer code or replays requests.
import {writeFile, mkdir, appendFile} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {pathToFileURL} from 'node:url';

export class StdoutStream {
  constructor({maxBytes=1024*1024,maxEvents=256}={}) {
    this.maxBytes=maxBytes;this.maxEvents=maxEvents;
    this.bytes=0;this.events=[];this.subscribed=false;
  }
  accept(message) {
    if (message.id===1) {
      if (this.subscribed || message.error || message.result?.type!=='Success')
        throw Error('Stdout subscription rejected or ambiguous');
      this.subscribed=true;return;
    }
    if (message.method!=='streamNotify' || message.params?.streamId!=='Stdout') return;
    const event=message.params.event;
    if (event?.kind!=='WriteEvent' || typeof event.bytes!=='string')
      throw Error('Invalid stdout event');
    const bytes=Buffer.from(event.bytes,'base64');
    if(bytes.toString('base64')!==event.bytes)throw Error('Invalid stdout base64');
    if(this.events.length>=this.maxEvents || this.bytes+bytes.length>this.maxBytes)
      throw Error('Stdout evidence capacity exceeded');
    const isolate=event.isolate?.id ?? null;
    if(isolate!==null && typeof isolate!=='string')throw Error('Invalid stdout isolate');
    this.events.push(structuredClone(event));this.bytes+=bytes.length;
  }
  snapshot() {
    const streams=new Map();
    for(const event of this.events) {
      const isolate=event.isolate?.id ?? null;
      if(!streams.has(isolate))streams.set(isolate,[]);
      streams.get(isolate).push(Buffer.from(event.bytes,'base64'));
    }
    return {subscribed:this.subscribed,bytes:this.bytes,event_count:this.events.length,
      streams:[...streams].map(([isolate,chunks])=>({isolate,
        text:new TextDecoder('utf-8',{fatal:true}).decode(Buffer.concat(chunks))}))};
  }
}

async function run(endpoint,output,seconds) {
  const url=new URL(endpoint);
  if(url.protocol!=='ws:' || !['127.0.0.1','localhost','[::1]'].includes(url.hostname)
      || !Number.isFinite(seconds) || seconds<=0 || seconds>900)
    throw Error('Expected owned loopback WebSocket and duration 1..900 seconds');
  await mkdir(output);
  const stream=new StdoutStream();
  const socket=new WebSocket(url);
  let failure=null,reason=null,finished=false;
  let writes=Promise.resolve();
  const started=Date.now();
  const persist=async()=>{
    await writeFile(`${output}/events.json`,JSON.stringify(stream.events));
    let snapshot;
    try {snapshot=stream.snapshot();} catch(error) {failure??=error.message;snapshot={bytes:stream.bytes,event_count:stream.events.length};}
    await writeFile(`${output}/result.json`,JSON.stringify({...snapshot,reason,error:failure},null,2));
  };
  await new Promise(resolve=>{
    const finish=(why,error=null)=>{
      if(finished)return;finished=true;reason=why;failure=error ?? (stream.subscribed ? null : 'No confirmed stdout subscription');
      clearInterval(timer);socket.close();resolve();
    };
    const timer=setInterval(()=>{
      if(existsSync(`${output}/stop`))finish('caller_stop');
      else if(!stream.subscribed && Date.now()-started>10000)finish('subscription_timeout','No confirmed subscription');
      else if(Date.now()-started>=seconds*1000)finish('duration');
    },25);
    socket.addEventListener('open',()=>socket.send(JSON.stringify({jsonrpc:'2.0',id:1,method:'streamListen',params:{streamId:'Stdout'}})));
    socket.addEventListener('message',event=>{
      if(finished)return;
      try {
        if(typeof event.data!=='string' || event.data.length>2*1024*1024)throw Error('Oversize or binary VM message');
        const before=stream.subscribed,count=stream.events.length;stream.accept(JSON.parse(event.data));
        if(stream.events.length>count) {
          const retained=JSON.stringify(stream.events.at(-1))+'\n';
          writes=writes.then(()=>appendFile(`${output}/events.jsonl`,retained));
        }
        if(!before && stream.subscribed)writes=writes.then(()=>writeFile(`${output}/ready.json`,JSON.stringify({subscribed:true})));
        writes.catch(e=>finish('evidence_error',e.message));
      } catch(error) {finish('protocol_or_capacity_error',error.message);}
    });
    socket.addEventListener('error',()=>finish('transport_error','VM service transport failed'));
    socket.addEventListener('close',()=>finish('transport_closed','VM service closed before caller stop'));
  });
  try {await writes;} catch(error) {failure??=error.message;}
  await persist();
  if(failure)throw Error(failure);
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href) {
  const [endpoint,output,duration='120']=process.argv.slice(2);
  await run(endpoint,output,Number(duration));
}
