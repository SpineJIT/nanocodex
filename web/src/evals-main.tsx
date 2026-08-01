import { createRoot } from "react-dom/client";
import { LiveEvals, useLiveEvalSnapshot } from "./LiveEvals";
import "./index.css";

function LiveEvalsApp() {
  const connection = useLiveEvalSnapshot();
  if (connection.availability === "available" && connection.snapshot) {
    return <LiveEvals connection={connection} />;
  }
  return (
    <main className="live-evals-boot page-grid">
      <p className="eyebrow">Nanocodex · retained evidence</p>
      <h1>{connection.availability === "checking" ? "Connecting to evals…" : "Evals unavailable"}</h1>
      <p>
        {connection.availability === "checking"
          ? "Loading the durable task matrix and current host health."
          : "The evidence server is not reachable. Retrying automatically…"}
      </p>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(<LiveEvalsApp />);
