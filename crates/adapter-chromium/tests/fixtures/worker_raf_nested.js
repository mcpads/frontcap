// The parent stays idle while its child owns the next timer.
new Worker("worker_raf.js").onmessage = e => postMessage(e.data);
