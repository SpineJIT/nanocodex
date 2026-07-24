import { Provider, Storage } from "accounts";
import { createJsonChannelStore, tempo } from "mppx/client";

import { PATH_USD } from "./tempo-policy";
import { hydrateTempoAccessKey, type TempoAccessKey } from "./tempoAccessKey";

export async function createTempoMppSession(key: TempoAccessKey) {
  const provider = Provider.create({ mpp: false, storage: Storage.idb() });
  const account = hydrateTempoAccessKey(key);

  const storage = Storage.idb({ key: "nanocodex-mpp-channels" });
  const channelStore = createJsonChannelStore({
    async get(key) { return (await storage.getItem<string>(key)) ?? undefined; },
    async set(key, value) { await storage.setItem(key, value); },
    async delete(key) { await storage.removeItem(key); },
  });
  const mpp = tempo.session.manager({
    account,
    autoSwap: { tokenIn: [PATH_USD], slippage: 1 },
    bootstrap: true,
    channelStore,
    client: provider.getClient(),
    maxDeposit: "0.05",
    topUpAmount: "0.05",
  });
  return {
    mpp,
    rootAddress: key.rootAddress,
    accessKeyAddress: account.accessKeyAddress,
  };
}
