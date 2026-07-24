import { Provider, Storage } from "accounts";
import { createJsonChannelStore, tempo } from "mppx/client";
import { parseUnits } from "viem";
import type { Account as TempoAccount } from "viem/tempo";

import { PATH_USD, USDC_E } from "./tempo-policy";

const requiredLimits = [
  { token: PATH_USD, limit: parseUnits("25", 6) },
  { token: USDC_E, limit: parseUnits("25", 6) },
];

type AccessKeyRecord = {
  address: `0x${string}`;
  limits?: readonly { token: `0x${string}`; limit: bigint }[];
};

type AccountsStore = {
  accessKeys: {
    get(query: { account: `0x${string}`; accessKey: `0x${string}`; chainId: number }): Promise<TempoAccount.Account | undefined>;
    list(query: { account: `0x${string}`; chainId: number }): readonly AccessKeyRecord[];
  };
  persist: {
    hasHydrated(): boolean;
    onFinishHydration(listener: () => void): () => void;
  };
};

type AccountsProvider = Omit<Provider.Provider, "store"> & { store: AccountsStore };

export async function createTempoMppSession() {
  const provider = Provider.create({ mpp: false, storage: Storage.idb() }) as unknown as AccountsProvider;
  await waitForHydration(provider.store);
  const root = provider.getAccount();
  const record = provider.store.accessKeys
    .list({ account: root.address, chainId: provider.getClient().chain.id })
    .find((key) => requiredLimits.every((required) => key.limits?.some((limit) =>
      limit.token.toLowerCase() === required.token.toLowerCase() && limit.limit >= required.limit,
    )));
  if (!record) throw new Error("Authorize the Tempo access key in this page first");
  const account = await provider.store.accessKeys.get({
    account: root.address,
    accessKey: record.address,
    chainId: provider.getClient().chain.id,
  });
  if (!account?.accessKeyAddress) throw new Error("Tempo Accounts did not load the authorized access key");

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
  return { mpp, rootAddress: root.address, accessKeyAddress: account.accessKeyAddress };
}

async function waitForHydration(store: AccountsStore) {
  if (store.persist.hasHydrated()) return;
  await new Promise<void>((resolve) => {
    let settled = false;
    let unsubscribe = () => {};
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      unsubscribe();
      resolve();
    };
    const timeout = setTimeout(finish, 1_000);
    unsubscribe = store.persist.onFinishHydration(() => {
      finish();
    });
  });
}
