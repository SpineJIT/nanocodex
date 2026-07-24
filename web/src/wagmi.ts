import { createConfig, http } from "wagmi";
import { tempo } from "wagmi/chains";
import { tempoWallet } from "wagmi/connectors";

export const wagmiConfig = createConfig({
  chains: [tempo],
  connectors: [
    tempoWallet({
      // Tempo Wallet owns account/passkey UX. The app Worker owns only the
      // long-lived MPP session and channel store. A payment-scoped key is
      // authorized separately so mppx can raw-sign streaming vouchers.
      mpp: false,
    }),
  ],
  multiInjectedProviderDiscovery: false,
  transports: { [tempo.id]: http() },
});

declare module "wagmi" {
  interface Register {
    config: typeof wagmiConfig;
  }
}
