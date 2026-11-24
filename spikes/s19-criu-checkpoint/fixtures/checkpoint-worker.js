(() => {
  // The stock engine does not run worker timers on virtual time, so the heap is read by message.
  let secret = 0;
  onmessage = ({data}) => {
    if (data === 'step') secret++;
    if (data === 'read') postMessage(secret);
  };
  postMessage('ready');
})();
