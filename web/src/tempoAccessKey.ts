import { Storage } from "accounts";
import { KeyAuthorization } from "ox/tempo";
import { parseUnits, type Address } from "viem";
import { generatePrivateKey } from "viem/accounts";
import { Account, KeyAuthorizationManager } from "viem/tempo";
import { getConnectorClient } from "wagmi/actions";

import { MPP_ACCESS_KEY_LIMIT, PATH_USD, TEMPO_ESCROW } from "./tempo-policy";
import { wagmiConfig } from "./wagmi";

export type TempoAccessKey = {
  address: Address;
  chainId: number;
  expiresAt: number;
  keyAuthorization?: KeyAuthorization.Rpc;
  privateKey: `0x${string}`;
  rootAddress: Address;
};

const storage = Storage.idb({ key: "nanocodex-mpp-access-keys" });
const ttlSeconds = 24 * 60 * 60;

/** Authorize or reuse the least-privilege signer used for MPP channel traffic. */
export async function provisionTempoAccessKey(): Promise<TempoAccessKey> {
  const client = await getConnectorClient(wagmiConfig);
  if (!client.account) throw new Error("No Tempo Wallet account is connected");
  const rootAddress = client.account.address;
  const chainId = client.chain.id;
  const existing = await load(rootAddress);
  if (existing && existing.chainId === chainId && existing.expiresAt > now()) return existing;

  const privateKey = generatePrivateKey();
  const address = Account.fromSecp256k1(privateKey).address;
  const expiresAt = now() + ttlSeconds;
  const result = await client.request({
    method: "wallet_authorizeAccessKey",
    params: [{
      address,
      chainId: BigInt(chainId),
      expiry: expiresAt,
      keyType: "secp256k1",
      limits: [{ limit: parseUnits(MPP_ACCESS_KEY_LIMIT, 6), token: PATH_USD }],
      scopes: [
        { address: PATH_USD, recipients: [TEMPO_ESCROW], selector: "approve(address,uint256)" },
        { address: TEMPO_ESCROW, selector: "open(address,address,uint128,bytes32,address)" },
        { address: TEMPO_ESCROW, selector: "topUp(bytes32,uint256)" },
      ],
      showDeposit: { amount: MPP_ACCESS_KEY_LIMIT, token: "pathUSD" },
    }],
  } as never) as { keyAuthorization: KeyAuthorization.Rpc };
  const key = {
    address,
    chainId,
    expiresAt,
    keyAuthorization: result.keyAuthorization,
    privateKey,
    rootAddress,
  } satisfies TempoAccessKey;
  await save(key);
  return key;
}

/** Recreate the access-key account inside the Agent Worker. */
export function hydrateTempoAccessKey(initial: TempoAccessKey) {
  let key = initial;
  const manager = KeyAuthorizationManager.from({
    source: {
      get(query) {
        if (!matches(key, query)) return undefined;
        return key.keyAuthorization
          ? KeyAuthorization.fromRpc(key.keyAuthorization) as never
          : undefined;
      },
      async set(query, authorization) {
        if (!matches(key, query)) return;
        key = { ...key, keyAuthorization: KeyAuthorization.toRpc(authorization) };
        await save(key);
      },
      async remove(query) {
        if (!matches(key, query)) return;
        const { keyAuthorization: _removed, ...rest } = key;
        key = rest;
        await save(key);
      },
    },
  });
  return Account.fromSecp256k1(key.privateKey, {
    access: key.rootAddress,
    keyAuthorizationManager: manager,
  });
}

function matches(key: TempoAccessKey, query: { address: Address; accessKey: Address; chainId: number }) {
  return key.chainId === query.chainId
    && key.rootAddress.toLowerCase() === query.address.toLowerCase()
    && key.address.toLowerCase() === query.accessKey.toLowerCase();
}

async function load(rootAddress: Address): Promise<TempoAccessKey | null> {
  return await storage.getItem<TempoAccessKey>(location(rootAddress));
}

async function save(key: TempoAccessKey): Promise<void> {
  await storage.setItem(location(key.rootAddress), key);
}

function location(rootAddress: Address) {
  return `key:${rootAddress.toLowerCase()}`;
}

function now() {
  return Math.floor(Date.now() / 1_000);
}
