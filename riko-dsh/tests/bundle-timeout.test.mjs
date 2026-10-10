import assert from "node:assert/strict";
import { createServer } from "node:http";
import test from "node:test";
import { MemoryClient } from "../dist/client.js";

test("context bundle waits beyond the removed 800ms cutoff", async (t) => {
  const server = createServer((_request, response) => {
    setTimeout(() => {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ request_id: "slow-bundle", resident: { text: "local fixture" } }));
    }, 1000);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  }));

  const address = server.address();
  assert.ok(address && typeof address === "object");
  const client = new MemoryClient({
    baseUrl: `http://127.0.0.1:${address.port}`,
    token: "local-test-token",
    writeTimeoutMs: 3000,
    composeTimeoutMs: 500,
    soulTimeoutMs: 300,
  });

  const result = await client.contextBundle({ agent_id: "test-agent", query: "local test" });
  assert.equal(result.status, 200);
  assert.equal(result.failure, "ok");
  assert.equal(result.requestId, "slow-bundle");
});
