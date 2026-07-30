import { registry } from "./registry.js";
import { startWebClient } from "./web-server.js";

const web = await startWebClient({
  ...(process.env.NANOCODEX_WEB_HOST === undefined
    ? {}
    : { host: process.env.NANOCODEX_WEB_HOST }),
  ...(process.env.NANOCODEX_WEB_PORT === undefined
    ? {}
    : { port: Number(process.env.NANOCODEX_WEB_PORT) }),
});
process.stderr.write(`Nanocodex browser client: ${web.url}\n`);
try {
  await registry.startAndWait();
} finally {
  await web.close();
}
