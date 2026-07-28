import { subscribeAgentEvents } from "../internal.mjs";

const MAX_BUFFERED_EVENTS = 4_096;
const MAX_BUFFERED_EVENT_CHARACTERS = 32 * 1024 * 1024;

export function watch(agent, options = {}) {
  const listeners = new Set();
  const iterators = new Set();
  let unsubscribe;
  let closed = false;

  const emit = (event, encodedLength) => {
    for (const listener of listeners) listener(event);
    for (const iterator of iterators) iterator.push(event, encodedLength);
  };

  const start = () => {
    if (closed || unsubscribe) return;
    unsubscribe = subscribeAgentEvents(agent, emit, options);
  };

  const stopIfIdle = () => {
    if (listeners.size || iterators.size) return;
    unsubscribe?.();
    unsubscribe = undefined;
  };

  const watcher = {
    onEvent(listener) {
      if (typeof listener !== "function") throw new TypeError("events.watch.onEvent requires a listener");
      if (closed) return () => {};
      listeners.add(listener);
      start();
      return () => {
        listeners.delete(listener);
        stopIfIdle();
      };
    },
    off() {
      if (closed) return;
      closed = true;
      unsubscribe?.();
      unsubscribe = undefined;
      listeners.clear();
      for (const iterator of [...iterators]) iterator.end();
      iterators.clear();
    },
    [Symbol.asyncIterator]() {
      if (closed) return emptyIterator();
      const iterator = eventIterator(() => {
        iterators.delete(iterator);
        stopIfIdle();
      });
      iterators.add(iterator);
      start();
      return iterator;
    },
  };
  return Object.freeze(watcher);
}

function eventIterator(onEnd) {
  const queue = [];
  let head = 0;
  let bufferedCharacters = 0;
  let pending;
  let ended = false;
  let failure;
  let failureReported = false;
  let detached = false;

  const detach = () => {
    if (detached) return;
    detached = true;
    onEnd();
  };

  const iterator = {
    push(event, encodedLength) {
      if (ended || failure) return;
      if (pending) {
        const resolve = pending;
        pending = undefined;
        resolve({ done: false, value: event });
      } else {
        const characters = encodedLength ?? JSON.stringify(event).length;
        if (
          queue.length - head >= MAX_BUFFERED_EVENTS
          || bufferedCharacters + characters > MAX_BUFFERED_EVENT_CHARACTERS
        ) {
          failure = new RangeError(
            `event iterator exceeded its private buffer of ${MAX_BUFFERED_EVENTS} events or `
              + `${MAX_BUFFERED_EVENT_CHARACTERS} serialized characters`,
          );
          detach();
          return;
        }
        queue.push({ characters, event });
        bufferedCharacters += characters;
      }
    },
    end() {
      if (ended) return;
      ended = true;
      detach();
      pending?.({ done: true, value: undefined });
      pending = undefined;
      queue.length = 0;
      head = 0;
      bufferedCharacters = 0;
    },
    next() {
      if (head < queue.length) {
        const entry = queue[head++];
        bufferedCharacters -= entry.characters;
        if (head === queue.length) {
          queue.length = 0;
          head = 0;
        }
        return Promise.resolve({ done: false, value: entry.event });
      }
      if (failure && !failureReported) {
        failureReported = true;
        return Promise.reject(failure);
      }
      if (ended || failure) return Promise.resolve({ done: true, value: undefined });
      return new Promise((resolve) => { pending = resolve; });
    },
    return() {
      iterator.end();
      return Promise.resolve({ done: true, value: undefined });
    },
    [Symbol.asyncIterator]() { return this; },
  };
  return iterator;
}

function emptyIterator() {
  return {
    next: () => Promise.resolve({ done: true, value: undefined }),
    return: () => Promise.resolve({ done: true, value: undefined }),
    [Symbol.asyncIterator]() { return this; },
  };
}
