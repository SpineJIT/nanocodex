import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { formatUnits } from "viem";
import { WagmiProvider, useConnect, useConnection, useConnectors, useDisconnect } from "wagmi";
import { tempo } from "wagmi/chains";
import { Hooks } from "wagmi/tempo";

import type { PaymentStatus } from "./nanocodex";
import { PATH_USD } from "./tempo-policy";
import { wagmiConfig } from "./wagmi";

const queryClient = new QueryClient();

export function MppControls(props: {
  jsonl: readonly string[];
  payment?: PaymentStatus;
  onDisconnect(): void;
  onReady(): void;
}) {
  return (
    <WagmiProvider config={wagmiConfig}>
      <QueryClientProvider client={queryClient}>
        <ConnectedMppControls {...props} />
      </QueryClientProvider>
    </WagmiProvider>
  );
}

function ConnectedMppControls({ jsonl, payment, onDisconnect, onReady }: {
  jsonl: readonly string[];
  payment?: PaymentStatus;
  onDisconnect(): void;
  onReady(): void;
}) {
  const connection = useConnection();
  const connectors = useConnectors();
  const connect = useConnect();
  const disconnect = useDisconnect();
  const reportedAddress = useRef<string | undefined>(undefined);
  const balance = Hooks.token.useGetBalance({
    account: connection.address,
    token: PATH_USD,
    query: {
      enabled: connection.status === "connected",
      refetchInterval: 5_000,
    },
  });

  useEffect(() => {
    if (connection.status !== "connected" || !connection.address) {
      reportedAddress.current = undefined;
      return;
    }
    if (reportedAddress.current === connection.address) return;
    // Wagmi restores persisted connections inside its hydration boundary.
    // Start the external agent after that render commits so its synchronous
    // store notification cannot update this tree while Hydrate is rendering.
    const address = connection.address;
    const timeout = window.setTimeout(() => {
      reportedAddress.current = address;
      onReady();
    }, 0);
    return () => window.clearTimeout(timeout);
  }, [connection.address, connection.status, onReady]);

  const connector = connectors[0];
  const connecting = connect.status === "pending";
  return (
    <aside className="agent-byok agent-mpp" aria-label="Tempo MPP payment">
      <div className="agent-byok-summary">
        <span>
          <i className={connection.status === "connected" ? "is-ready" : ""} aria-hidden="true" />
          {connection.status === "connected" ? "Tempo Wallet connected" : "Use Tempo Wallet for MPP"}
        </span>
        <div>
          {connection.status === "connected" ? (
            <button type="button" onClick={() => {
              onDisconnect();
              disconnect.mutate();
            }}>Disconnect</button>
          ) : (
            <button
              type="button"
              disabled={!connector || connecting}
              onClick={() => connector && connect.mutate({ connector, chainId: tempo.id })}
            >
              {connecting ? "Opening Tempo Wallet…" : "Continue with Tempo Wallet"}
            </button>
          )}
        </div>
      </div>
      {connect.error ? <p className="agent-byok-error" role="alert">{connect.error.message}</p> : null}
      {connection.status === "connected" ? (
        <dl className="agent-mpp-details">
          <Detail label="Tempo account" value={connection.address} />
          <Detail label="Payer" value={payment?.rootAddress ?? "Loading Tempo account…"} />
          <Detail label="Balance" value={balance.data === undefined ? "Loading…" : `${balance.data.formatted} pathUSD`} />
          <Detail label="Signer" value={payment?.accessKeyAddress ?? "Loading authorized access key…"} />
          <Detail label="Channel" value={payment?.channelId ?? "Opens on first paid request"} />
          <Detail label="Cumulative" value={payment ? `${formatUnits(BigInt(payment.cumulative), 6)} pathUSD` : "0 pathUSD"} />
        </dl>
      ) : null}
      {jsonl.length ? (
        <details className="agent-mpp-jsonl">
          <summary>MPP run JSONL ({jsonl.length})</summary>
          <pre>{jsonl.join("\n")}</pre>
        </details>
      ) : null}
    </aside>
  );
}

function Detail({ label, value }: { label: string; value: string | undefined }) {
  return <><dt>{label}</dt><dd title={value}>{value}</dd></>;
}
