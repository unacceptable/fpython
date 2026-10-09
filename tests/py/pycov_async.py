"""Coroutine coverage for the native Python compiler."""
import asyncio


class Lock:
    def __init__(self, log):
        self.log = log

    async def __aenter__(self):
        self.log.append("acquire")
        await asyncio.sleep(0)
        return self

    async def __aexit__(self, t, e, tb):
        self.log.append("release " + (t.__name__ if t else "ok"))
        return False


class Ticker:
    def __init__(self, n):
        self.i = 0
        self.n = n

    def __aiter__(self):
        return self

    async def __anext__(self):
        await asyncio.sleep(0)
        if self.i >= self.n:
            raise StopAsyncIteration
        self.i += 1
        return self.i


class Awaitable:
    def __await__(self):
        yield
        return "custom-await"


async def double(x):
    await asyncio.sleep(0)
    return x * 2


async def failing():
    await asyncio.sleep(0)
    raise ValueError("async boom")


async def nested_flow(log):
    total = 0
    async with Lock(log) as l1, Lock(log):
        async for i in Ticker(4):
            if i == 2:
                continue
            if i == 4:
                break
            total += await double(i)
        else:
            log.append("no-break")
    try:
        async with Lock(log):
            await failing()
    except ValueError as e:
        log.append("caught " + str(e))
    finally:
        log.append("finally")
    async for j in Ticker(2):
        pass
    else:
        log.append("for-else")
    return total


async def main():
    log = []
    total = await nested_flow(log)
    custom = await Awaitable()
    results = await asyncio.gather(*[double(i) for i in range(4)])
    task = asyncio.ensure_future(double(21))
    answer = await task
    waited = await asyncio.wait_for(double(5), timeout=1)
    lam = await (lambda: double(7))()
    comp = [await double(i) for i in range(3) if i != 1]
    dcomp = {i: await double(i) for i in range(2)}
    return total, custom, results, answer, waited, lam, comp, dcomp, log


def run_all():
    return repr(asyncio.run(main()))
