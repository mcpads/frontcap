(() => {
  let secret = 0;
  onmessage = ({data}) => {
    if (data === 'step') secret++;
    if (data === 'arm') setTimeout(() => postMessage(secret), 300);
  };
  postMessage('ready');
})();
