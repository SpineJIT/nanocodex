import {
  createContext,
  createElement,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useSyncExternalStore,
} from "react";

export { createConfig } from "./config.mjs";

const NanocodexContext = createContext(null);

export function NanocodexProvider({ children, config }) {
  if (!config) throw new TypeError("NanocodexProvider requires a config");
  useEffect(() => config.mount(), [config]);
  return createElement(NanocodexContext.Provider, { value: config }, children);
}

export function useConfig() {
  const config = useContext(NanocodexContext);
  if (!config) throw new Error("Nanocodex hooks must be used inside NanocodexProvider");
  return config;
}

export function useNanocodex() {
  const config = useConfig();
  const snapshot = useSyncExternalStore(config.subscribe, config.getSnapshot, config.getSnapshot);
  return useMemo(() => ({
    ...snapshot,
    dispatch: config.dispatch,
    start: config.start,
    restart: config.restart,
    disconnect: config.disconnect,
    stop: config.stop,
  }), [config, snapshot]);
}

export function useNanocodexMessage(listener) {
  const config = useConfig();
  const latest = useRef(listener);
  latest.current = listener;
  useEffect(
    () => config.subscribeMessages((message) => latest.current(message)),
    [config],
  );
}
