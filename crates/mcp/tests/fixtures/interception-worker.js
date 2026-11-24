setTimeout(() => fetch('https://api.example.test/worker').then((r) => r.text()).then((t) => postMessage(t)), 100);
