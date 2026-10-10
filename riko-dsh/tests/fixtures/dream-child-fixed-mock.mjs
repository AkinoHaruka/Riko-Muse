/**
 * Local fixed-response endpoints for official DSH Dream-child smoke checks.
 * They accept only loopback traffic and contain synthetic test logic; they do
 * not prove real model connectivity or semantic quality.
 */
import { createServer } from "node:http";
import { appendFileSync, writeFileSync } from "node:fs";

const chatPort = Number(process.env.D6_CHAT_PORT ?? 19682);
const embeddingPort = Number(process.env.D6_EMBEDDING_PORT ?? 19683);
const dataDir = process.env.D6_MOCK_DATA_DIR ?? process.cwd();
const logPath = `${dataDir}/chat-requests.jsonl`;
const statePath = `${dataDir}/chat-state.json`;
let sequence = 0;
let releaseParent = false;
const waitingParents = [];

function messageText(message) {
  const blocks = Array.isArray(message.content)
    ? message.content
    : [{ type: "text", text: String(message.content ?? "") }];
  return blocks.filter((block) => block.type === "text").map((block) => block.text ?? "").join("\n");
}

function allText(messages) {
  return messages.map(messageText).join("\n");
}

function parseTrailingObject(text) {
  for (let index = text.length - 1; index >= 0; index -= 1) {
    if (text[index] !== "{") continue;
    try {
      const value = JSON.parse(text.slice(index).trim());
      if (value && typeof value === "object" && !Array.isArray(value)) return value;
    } catch { /* keep searching for the start of the trailing JSON object */ }
  }
  return {};
}

function parseToolResult(block) {
  const blocks = Array.isArray(block?.content)
    ? block.content
    : [{ type: "text", text: String(block?.content ?? "") }];
  const raw = blocks.filter((item) => item.type === "text").map((item) => item.text ?? "").join("\n");
  try { return JSON.parse(raw); } catch { return undefined; }
}

function latestToolResult(messages, toolName) {
  const calls = [];
  for (const message of messages) {
    if (message.role !== "assistant" || !Array.isArray(message.content)) continue;
    for (const block of message.content) {
      if (block.type === "tool_use" && block.name === toolName) calls.push(block);
    }
  }
  const call = calls.at(-1);
  if (!call) return undefined;
  for (const message of messages) {
    if (message.role !== "user" || !Array.isArray(message.content)) continue;
    for (const block of message.content) {
      if (block.type === "tool_result" && block.tool_use_id === call.id) return parseToolResult(block);
    }
  }
  return undefined;
}

function sendMessage(response, blocks, stopReason = "end_turn") {
  response.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "close",
  });
  const emit = (event, data) => response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  emit("message_start", {
    type: "message_start",
    message: {
      id: `fixed_${sequence}`,
      type: "message",
      role: "assistant",
      model: "fixed-local",
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: { input_tokens: 1, output_tokens: 1 },
    },
  });
  for (const [index, block] of blocks.entries()) {
    emit("content_block_start", { type: "content_block_start", index, content_block: block });
    if (block.type === "text") {
      emit("content_block_delta", {
        type: "content_block_delta",
        index,
        delta: { type: "text_delta", text: block.text },
      });
    }
    emit("content_block_stop", { type: "content_block_stop", index });
  }
  emit("message_delta", {
    type: "message_delta",
    delta: { stop_reason: stopReason, stop_sequence: null },
    usage: { output_tokens: 2 },
  });
  emit("message_stop", { type: "message_stop" });
  response.end();
}

function toolUse(response, name, input) {
  sequence += 1;
  sendMessage(response, [{ type: "tool_use", id: `fixed_tool_${sequence}`, name, input }], "tool_use");
}

function textReply(response, text) {
  sendMessage(response, [{ type: "text", text }]);
}

function writeRecord(record) {
  appendFileSync(logPath, `${JSON.stringify(record)}\n`);
  writeFileSync(statePath, JSON.stringify(record, null, 2));
}

function releaseWaitingParents() {
  releaseParent = true;
  while (waitingParents.length > 0) textReply(waitingParents.shift(), "fixed parent completion");
}

const chatServer = createServer(async (request, response) => {
  if (request.method === "POST" && request.url === "/release") {
    releaseWaitingParents();
    response.writeHead(200).end("released");
    return;
  }
  if (request.method === "GET" && request.url === "/state") {
    response.writeHead(200, { "content-type": "application/json" })
      .end(JSON.stringify({ releaseParent, sequence }));
    return;
  }
  if (request.method !== "POST" || !request.url.endsWith("/messages")) {
    response.writeHead(404).end("not found");
    return;
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { response.writeHead(400).end("bad json"); return; }

  const messages = Array.isArray(body.messages) ? body.messages : [];
  const combinedText = allText(messages);
  const phase = combinedText.includes("restricted memory extraction child") ? "extract"
    : combinedText.includes("restricted memory adjudication child") ? "adjudicate"
      : combinedText.includes("restricted derived-page writer") ? "consolidate" : "parent";
  const tools = Array.isArray(body.tools) ? body.tools.map((tool) => tool.name) : [];
  const systemText = typeof body.system === "string" ? body.system
    : Array.isArray(body.system) ? body.system.map((block) => block?.text ?? "").join("\n") : "";
  const record = {
    sequence: ++sequence,
    phase,
    tool_names: tools,
    system_has_soul: systemText.includes("agent_memory:soul"),
    assistant_tool_names: messages.flatMap((message) => Array.isArray(message.content) ? message.content : [])
      .filter((block) => block.type === "tool_use").map((block) => block.name),
  };
  writeRecord(record);

  if (phase === "parent") {
    if (releaseParent) textReply(response, "fixed parent completion");
    else waitingParents.push(response);
    return;
  }

  if (phase === "extract") {
    const manifest = latestToolResult(messages, "dream_read_manifest");
    const evidence = latestToolResult(messages, "dream_read_evidence");
    if (!manifest) {
      if (!tools.includes("dream_read_manifest")) { textReply(response, "missing dream_read_manifest"); return; }
      toolUse(response, "dream_read_manifest", {});
      return;
    }
    if (!evidence) {
      const ids = manifest.data?.manifest?.frozen_evidence?.map((item) => item.evidence_id) ?? [];
      if (ids.length === 0 || !tools.includes("dream_read_evidence")) { textReply(response, "missing frozen evidence"); return; }
      toolUse(response, "dream_read_evidence", { ids });
      return;
    }
    const source = evidence.data?.evidence?.find((item) => item.role === "user");
    const taskPrompt = messages.map(messageText)
      .find((text) => text.includes("restricted memory extraction child")) ?? "";
    if (!source) { textReply(response, "missing user evidence"); return; }
    toolUse(response, "structured_output", {
      candidates: [{ evidence_id: source.evidence_id, kind: "fact", quote: source.content, claim: source.content }],
    });
    return;
  }

  if (phase === "adjudicate") {
    const manifest = latestToolResult(messages, "dream_read_manifest");
    const evidence = latestToolResult(messages, "dream_read_evidence");
    const search = latestToolResult(messages, "dream_search_memories");
    const taskPrompt = messages.map(messageText)
      .find((text) => text.includes("restricted memory adjudication child")) ?? "";
    const candidateId = taskPrompt.match(/"candidate_id"\s*:\s*"([^"]+)"/)?.[1];
    if (!candidateId) { textReply(response, "missing candidate id"); return; }
    if (!manifest) { toolUse(response, "dream_read_manifest", {}); return; }
    if (!evidence) {
      const ids = manifest.data?.manifest?.frozen_evidence?.map((item) => item.evidence_id) ?? [];
      toolUse(response, "dream_read_evidence", { ids });
      return;
    }
    if (!search) {
      if (!tools.includes("dream_search_memories")) { textReply(response, "missing dream_search_memories"); return; }
      toolUse(response, "dream_search_memories", {
        query: "星岚系统 Rust 服务端开发",
        candidate_id: candidateId,
        limit: 5,
      });
      return;
    }
    toolUse(response, "structured_output", {
      results: [{
        candidate_id: candidateId,
        durability: "durable",
        action: "create",
        reason_code: null,
        target_memory_id: null,
        expected_target_version: null,
        model_confidence: 0.9,
        valid_until: null,
      }],
    });
    return;
  }

  if (phase === "consolidate") {
    const pageSearch = latestToolResult(messages, "dream_search_pages");
    const pageDetail = latestToolResult(messages, "dream_get_page");
    if (!pageSearch) {
      if (!tools.includes("dream_search_pages")) { textReply(response, "missing dream_search_pages"); return; }
      toolUse(response, "dream_search_pages", { query: "rust-work Rust 工作", limit: 5 });
      return;
    }
    if (pageSearch.data?.complete !== true || !Array.isArray(pageSearch.data?.items)) {
      textReply(response, "incomplete page search");
      return;
    }
    const existing = pageSearch.data.items[0];
    if (existing && !pageDetail) {
      if (!tools.includes("dream_get_page")) { textReply(response, "missing dream_get_page"); return; }
      toolUse(response, "dream_get_page", { id: existing.page_id });
      return;
    }
    if (existing && !pageDetail.data?.page) { textReply(response, "page detail unavailable"); return; }
    const updated = Boolean(existing);
    toolUse(response, "structured_output", {
      title: "Rust 服务端工作方式",
      description: updated
        ? "汇总用户的 Rust 服务端工作经历与沟通偏好。"
        : "用户在 Rust 服务端工作的经历与相关偏好。",
      body_md: updated
        ? "## 工作与协作\n\n用户从事 Rust 服务端开发，并偏好先给结论、表达简洁。"
        : "## 工作经历\n\n用户从事 Rust 服务端开发。",
    });
    return;
  }
  textReply(response, "unhandled fixed child phase");
});

const embeddingServer = createServer(async (request, response) => {
  if (request.method !== "POST" || !request.url.endsWith("/embeddings")) {
    response.writeHead(404).end("not found");
    return;
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { response.writeHead(400).end("{}"); return; }
  const input = Array.isArray(body.input) ? body.input : [body.input];
  const data = input.map((_, index) => ({ index, embedding: [0.1, 0.2, 0.3] }));
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify({ object: "list", model: body.model ?? "fixed-test-embedding", data }));
});

chatServer.listen(chatPort, "127.0.0.1", () => console.log(`fixed chat mock listening on ${chatPort}`));
embeddingServer.listen(embeddingPort, "127.0.0.1", () => console.log(`fixed embedding mock listening on ${embeddingPort}`));
