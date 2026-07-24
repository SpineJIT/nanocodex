export function createConfig(options) {
  if (typeof options?.worker !== "function") {
    throw new TypeError("createConfig requires worker()");
  }
  const stateListeners = new Set();
  const messageListeners = new Set();
  let snapshot = Object.freeze({ status: "idle", error: undefined });
  let worker;
  let mounts = 0;

  function setSnapshot(status, error) {
    if (snapshot.status === status && snapshot.error === error) return;
    snapshot = Object.freeze({ status, error });
    for (const listener of stateListeners) listener();
  }

  function defaultStartCommand() {
    return {
      type: "start",
      thinking: options.thinking ?? "high",
      reasoningMode: options.reasoningMode ?? "standard",
    };
  }

  function connect(command = defaultStartCommand()) {
    if (worker || snapshot.status === "stopped") return;
    setSnapshot("starting");
    try {
      const current = options.worker();
      worker = current;
      current.onmessage = ({ data }) => {
        if (data?.type === "ready") setSnapshot("ready");
        if (data?.type === "fatal") {
          setSnapshot("error", typeof data.message === "string" ? data.message : "Agent worker failed");
        }
        for (const listener of messageListeners) listener(data);
      };
      current.postMessage(command);
    } catch (error) {
      worker = undefined;
      setSnapshot("error", errorMessage(error));
    }
  }

  function disconnect() {
    worker?.terminate();
    worker = undefined;
    if (snapshot.status !== "stopped") setSnapshot("idle");
  }

  return Object.freeze({
    getSnapshot: () => snapshot,
    subscribe(listener) {
      stateListeners.add(listener);
      return () => stateListeners.delete(listener);
    },
    subscribeMessages(listener) {
      messageListeners.add(listener);
      return () => messageListeners.delete(listener);
    },
    mount() {
      mounts += 1;
      if (options.autoStart !== false) connect();
      let mounted = true;
      return () => {
        if (!mounted) return;
        mounted = false;
        mounts -= 1;
        if (mounts === 0) disconnect();
      };
    },
    dispatch(command) {
      if (!worker) throw new Error("the Nanocodex worker is not running");
      worker.postMessage(command);
    },
    start(command) {
      connect(command);
    },
    restart(command) {
      if (snapshot.status === "stopped") return;
      disconnect();
      connect(command);
    },
    disconnect,
    stop() {
      if (snapshot.status === "stopped") return;
      setSnapshot("stopped");
      disconnect();
    },
  });
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}
