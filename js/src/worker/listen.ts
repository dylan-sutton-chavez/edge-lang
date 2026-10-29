import * as engine from './engine.ts';
import { errMsg } from '../util.ts';
import type { WorkerRequest, WorkerMessage } from '../protocol.ts';
import type { Composition } from '../system/index.ts';
import type { TraceEvent } from '../system/trace.ts';

const post = (msg: WorkerMessage) => self.postMessage(msg);
const onLine = (text: string) => post({ type: 'line', text });
const onChannel = (channel: string, text: string) => post({ type: 'channel', channel, text });
const onTrace = (event: TraceEvent) => post({ type: 'trace', event });

/* one-way messages return this so no `response` is posted for them */
const NO_REPLY: unique symbol = Symbol('no-reply');

// the program files the page is reading each settled when its answer arrives
const reads = new Map<number, { resolve: (res: Response) => void, reject: (e: Error) => void }>();
let nextRead = 1;

/* asks the page for a program file since the room cannot reach the page origin */
const readFile = (url: string): Promise<Response> => new Promise((resolve, reject) => {
    const id = nextRead++;
    reads.set(id, { resolve, reject });
    post({ type: 'read', id, url });
});

function dispatch(req: WorkerRequest): unknown {
    switch (req.type) {
        case 'load': return engine.load(req.opts, readFile);
        case 'run': return engine.run({ src: req.src, repl: req.repl, entry: req.entry, incremental: req.incremental, input: req.input }, onLine, onTrace, onChannel);
        case 'set-preempt-interval': return engine.setPreemptInterval(req.interval);
        case 'pause': return engine.pause();
        case 'resume': return engine.resume();
        case 'save-state': return engine.saveState();
        case 'restore-state': return engine.restoreState({ blob: req.blob, onLine, onChannel });
        case 'state-globals': return engine.stateGlobals();
        case 'state-stack': return engine.stateStack();
        case 'reset': return engine.reset();
        case 'clear-cache': return engine.clearCache();
        case 'dispose': engine.dispose(); self.close(); return NO_REPLY;
        // wakes a paused `receive` in the running script
        case 'push-event': engine.pushEvent(req.message); return NO_REPLY;
        case 'file': {
            const waiting = reads.get(req.id);
            reads.delete(req.id);
            if (req.status === 0) waiting?.reject(new TypeError('the page could not read it'));
            else waiting?.resolve(new Response(req.body, { status: req.status, headers: { 'content-type': req.contentType } }));
            return NO_REPLY;
        }
        default: {
            // unreachable by the types and reached only when main and worker versions drift
            const _exhaustive: never = req;
            throw new Error(`unknown message type: ${JSON.stringify(_exhaustive)}`);
        }
    }
}

/* answers each request `createWorker` posts with the modules and grants the composition wires */
export function listen({ modules, access }: Composition): void {
    engine.wire(...modules);
    engine.access(access ?? null);
    self.onmessage = async ({ data }: MessageEvent<WorkerRequest>) => {
        try {
            const result = await dispatch(data);
            if (result === NO_REPLY) return;
            post({ type: 'response', reqId: data.reqId, result });
        } catch (e) {
            post({ type: 'error', reqId: data.reqId, message: errMsg(e) });
        }
    };
}
