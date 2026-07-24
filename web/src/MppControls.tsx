import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { formatUnits } from "viem";
import { WagmiProvider, useConnect, useConnection, useConnectors, useDisconnect } from "wagmi";
import { tempo } from "wagmi/chains";
import { Actions } from "wagmi/tempo";

import type { PaymentStatus } from "./nanocodex";
import { PATH_USD } from "./tempo-policy";
import { provisionTempoAccessKey, type TempoAccessKey } from "./tempoAccessKey";
import { wagmiConfig } from "./wagmi";

const queryClient = new QueryClient();

export function MppControls(props: {
  jsonl: readonly string[];
  payment?: PaymentStatus;
  onDisconnect(): void;
  onReady(key: TempoAccessKey): void;
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
  onReady(key: TempoAccessKey): void;
}) {
  const connection = useConnection();
  const connectors = useConnectors();
  const connect = useConnect();
  const disconnect = useDisconnect();
  const reportedAddress = useRef<string | undefined>(undefined);
  const [balance, setBalance] = useState<string>();
  const [accessKeyAddress, setAccessKeyAddress] = useState<string>();
  const [provisionError, setProvisionError] = useState("");
  const [provisioning, setProvisioning] = useState(false);
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    if (connection.status !== "connected" || !connection.address) {
      setBalance(undefined);
      return;
    }
    let active = true;
    const refresh = () => {
      void Actions.token.getBalance(wagmiConfig, {
        account: connection.address,
        token: PATH_USD,
      }).then((value) => {
        if (active) setBalance(value.formatted);
      });
    };
    refresh();
    const interval = window.setInterval(refresh, 5_000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [connection.address, connection.status]);

  useEffect(() => {
    if (connection.status !== "connected" || !connection.address) {
      reportedAddress.current = undefined;
      setAccessKeyAddress(undefined);
      setProvisionError("");
      setProvisioning(false);
      return;
    }
    if (reportedAddress.current === connection.address) return;
    let active = true;
    setProvisioning(true);
    setProvisionError("");
    void provisionTempoAccessKey().then((key) => {
      if (!active) return;
      reportedAddress.current = connection.address;
      setAccessKeyAddress(key.address);
      setProvisioning(false);
      onReady(key);
    }, (error) => {
      if (!active) return;
      setProvisioning(false);
      setProvisionError(error instanceof Error ? error.message : String(error));
    });
    return () => { active = false; };
  }, [connection.address, connection.status, onReady, retry]);

  const connector = connectors[0];
  const connecting = connect.status === "pending";
  return (
    <aside className="agent-byok agent-mpp" aria-label="Tempo MPP payment">
      <div className="agent-byok-summary">
        <span>
          <i className={connection.status === "connected" ? "is-ready" : ""} aria-hidden="true" />
          {connection.status === "connected"
            ? provisioning ? "Authorizing MPP access key…" : "Tempo Wallet connected"
            : "Use Tempo Wallet for MPP"}
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
      {provisionError ? (
        <p className="agent-byok-error" role="alert">
          {provisionError}{" "}
          <button type="button" onClick={() => setRetry((value) => value + 1)}>Retry</button>
        </p>
      ) : null}
      {connection.status === "connected" ? (
        <dl className="agent-mpp-details">
          <Detail label="Tempo account" value={connection.address} />
          <Detail label="Payer" value={payment?.rootAddress ?? connection.address} />
          <Detail label="Balance" value={balance === undefined ? "Loading…" : `${balance} pathUSD`} />
          <Detail label="Signer" value={payment?.accessKeyAddress ?? accessKeyAddress ?? "Authorizing payment key…"} />
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
