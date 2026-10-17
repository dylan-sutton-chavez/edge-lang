# The async API lang hands its languages, `from tasks import run, sleep, gather, with_timeout, cancel`, a loop over
# the coroutine protocol on a virtual clock, so a sleep takes no real time and every run prints the same.


class CancelledError(BaseException):
    pass


class _Wait:
    # What a task waits for, which the loop reads off the value the task yields.
    def __init__(self, kind, arg):
        self.kind = kind
        self.arg = arg

    def __await__(self):
        return (yield self)


class Task:
    # A coroutine the loop steps, done once it returns or raises, `token` the one wake that may resume it.
    def __init__(self, coro, loop):
        self.coro = coro
        self.loop = loop
        self.done = False
        self.result = None
        self.error = None
        self.waiters = []
        self.token = 0

    def __await__(self):
        if not self.done:
            yield _Wait('task', self)
        if self.error is not None:
            raise self.error
        return self.result

    def cancel(self):
        return cancel(self)


class _Loop:
    def __init__(self):
        # The ready queue is a list read from `head`, cut back once half of it is spent, so a step costs O(1).
        self.ready = []
        self.head = 0
        # Sleepers sit in a binary heap by wake time, then by the order they slept.
        self.timers = []
        self.seq = 0
        self.now = 0
        self.by_coro = {}

    def spawn(self, aw):
        if isinstance(aw, Task):
            return aw
        coro = aw if type(aw).__name__ == 'coroutine' else _wrap(aw)
        task = Task(coro, self)
        self.by_coro[id(coro)] = task
        self.wake(task, None, None)
        return task

    def wake(self, task, value, error):
        self.ready.append((task, task.token, value, error))

    def at(self, when, task):
        self.seq += 1
        _push(self.timers, (when, self.seq, task, task.token))

    def drive(self):
        while True:
            while self.head < len(self.ready):
                task, token, value, error = self.ready[self.head]
                self.ready[self.head] = None
                self.head += 1
                if self.head > 64 and self.head * 2 > len(self.ready):
                    self.ready = self.ready[self.head:]
                    self.head = 0
                if token == task.token and not task.done:
                    self.step(task, value, error)
            if not self.timers:
                return
            when, _, task, token = _pop(self.timers)
            self.now = max(self.now, when)
            if token == task.token:
                self.wake(task, None, None)

    def step(self, task, value, error):
        task.token += 1
        try:
            if error is not None:
                wait = task.coro.throw(error)
            else:
                wait = task.coro.send(value)
        except StopIteration as e:
            return self.finish(task, e.value, None)
        except BaseException as e:
            return self.finish(task, None, e)
        if not isinstance(wait, _Wait):
            return self.wake(task, None, RuntimeError('a task yielded ' + repr(wait) + ', await sleep, gather, with_timeout or a task'))
        if wait.kind == 'sleep' and wait.arg > 0:
            self.at(self.now + wait.arg, task)
        elif wait.kind == 'task' and not wait.arg.done:
            wait.arg.waiters.append((task, task.token))
        else:
            self.wake(task, None, None)

    def finish(self, task, result, error):
        task.done = True
        task.result = result
        task.error = error
        self.by_coro.pop(id(task.coro), None)
        for waiter, token in task.waiters:
            if waiter.token == token and not waiter.done:
                self.wake(waiter, None, None)
        task.waiters = []


_loops = []


def _running():
    if not _loops:
        raise RuntimeError('no running loop, start one with run()')
    return _loops[-1]


async def _wrap(aw):
    return await aw


def _push(heap, item):
    heap.append(item)
    i = len(heap) - 1
    while i > 0:
        parent = (i - 1) // 2
        if heap[parent] <= item:
            break
        heap[i] = heap[parent]
        i = parent
    heap[i] = item


def _pop(heap):
    top = heap[0]
    last = heap.pop()
    if heap:
        n = len(heap)
        i = 0
        while True:
            child = 2 * i + 1
            if child >= n:
                break
            if child + 1 < n and heap[child + 1] < heap[child]:
                child += 1
            if last <= heap[child]:
                break
            heap[i] = heap[child]
            i = child
        heap[i] = last
    return top


def run(*aws):
    # Runs the awaitables as tasks until every task ends, and gives what the first returned or raises what it raised.
    if _loops:
        raise RuntimeError('run() cannot be called from a running loop')
    loop = _Loop()
    _loops.append(loop)
    try:
        tasks = [loop.spawn(aw) for aw in aws]
        loop.drive()
    finally:
        _loops.pop()
    if not tasks:
        return None
    if tasks[0].error is not None:
        raise tasks[0].error
    return tasks[0].result


def sleep(seconds):
    # Suspends the task for `seconds` of the virtual clock, `sleep(0)` lets every ready task run once first.
    return _Wait('sleep', seconds)


async def gather(*aws):
    # Runs the awaitables at once and gives their results in order, the first error raised as it happens.
    loop = _running()
    tasks = [loop.spawn(aw) for aw in aws]
    return [await task for task in tasks]


async def with_timeout(seconds, aw):
    # Gives what `aw` returns, or raises TimeoutError once `seconds` pass first, cancelling it.
    loop = _running()
    task = loop.spawn(aw)
    fired = []

    async def watchdog():
        await sleep(seconds)
        if not task.done:
            fired.append(True)
            cancel(task)

    dog = loop.spawn(watchdog())
    try:
        return await task
    except CancelledError:
        if fired:
            raise TimeoutError('timed out after ' + str(seconds) + ' seconds')
        raise
    finally:
        cancel(dog)


def cancel(task):
    # Raises CancelledError in a running task or coroutine where it waits, which it may catch, False once it ended.
    loop = _running()
    if not isinstance(task, Task):
        task = loop.by_coro.get(id(task))
    if task is None or task.done:
        return False
    task.token += 1
    loop.wake(task, None, CancelledError())
    return True
