export const roomScript = `
addEventListener('message', function start({ source, data, ports: [port] }) {
    if (source !== parent || !port) return;
    removeEventListener('message', start);
    try {
        const url = URL.createObjectURL(new Blob([data], { type: 'application/javascript' }));
        // Chrome refuses a module worker from a blob in an opaque origin, the engine is a classic script.
        const worker = new Worker(url);
        setTimeout(() => URL.revokeObjectURL(url), 0);
        worker.onmessage = (e) => port.postMessage(e.data);
        worker.onerror = (e) => port.postMessage({ type: 'error', message: e.message || 'worker error' });
        port.onmessage = (e) => worker.postMessage(e.data);
    } catch (e) {
        port.postMessage({ type: 'error', message: 'the room could not start its worker, ' + e.message });
    }
});
`;
