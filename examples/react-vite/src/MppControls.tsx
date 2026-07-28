import {
  QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import {
  useCallback,
  useEffect,
  useMemo,
} from "react";
import { formatUnits } from "viem";
import {
  WagmiProvider,
  useConnect,
  useConnection,
  useConnectors,
} from "wagmi";
import { tempo } from "wagmi/chains";
import { Hooks } from "wagmi/tempo";

import { PATH_USD } from "./tempo-policy";
import { wagmiConfig } from "./wagmi";

export type MppConnection = {
  start(): Promise<string | undefined>;
  balance?: string;
};

const queryClient = new QueryClient();

export function MppControls({
  onChange,
}: {
  onChange(connection: MppConnection | undefined): void;
}) {
  return (
    <WagmiProvider config={wagmiConfig} reconnectOnMount={false}>
      <QueryClientProvider client={queryClient}>
        <ConnectedMppControls onChange={onChange} />
      </QueryClientProvider>
    </WagmiProvider>
  );
}

function ConnectedMppControls({
  onChange,
}: {
  onChange(connection: MppConnection | undefined): void;
}) {
  const connection = useConnection();
  const connectors = useConnectors();
  const connect = useConnect();
  const balance = Hooks.token.useGetBalance({
    account: connection.address,
    token: PATH_USD,
    query: {
      enabled: connection.status === "connected",
      refetchInterval: 5_000,
    },
  });
  const start = useCallback(async () => {
    const connector = connectors[0];
    if (!connector) throw new Error("Tempo Wallet connector is unavailable");
    const connected = connection.status === "connected"
      ? connection
      : await connect.mutateAsync({ connector, chainId: tempo.id });
    return "address" in connected
      ? connected.address
      : connected.accounts[0];
  }, [connect, connection, connectors]);
  const formattedBalance = balance.data === undefined
    ? undefined
    : formatUnits(balance.data.amount, 6);
  const mppConnection = useMemo(
    () => ({ start, balance: formattedBalance }),
    [formattedBalance, start],
  );

  useEffect(() => {
    onChange(mppConnection);
    return () => onChange(undefined);
  }, [mppConnection, onChange]);

  return null;
}
