import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once, EventEmitter } from 'node:events';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createInterface } from 'node:readline';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';

const binary = process.env.DEPPY_COMPLETION_FIXTURE;
assert.ok(binary, 'DEPPY_COMPLETION_FIXTURE must name the sole-Cargo-built private fixture');
const sdkPackage = JSON.parse(await readFile(new URL('./node_modules/@modelcontextprotocol/sdk/package.json', import.meta.url)));
const lockText = await readFile(new URL('./package-lock.json', import.meta.url), 'utf8');
const lock = JSON.parse(lockText);
assert.equal(sdkPackage.version, '1.32.1');
const sdkLock = lock.packages['node_modules/@modelcontextprotocol/sdk'];
assert.equal(sdkLock.version, sdkPackage.version);

const child = spawn(binary, [], { stdio: ['pipe', 'pipe', 'pipe'] });
const childExit = once(child, 'exit');
const events = new EventEmitter();
const fixtureEvents = [];
const requests = [];
const streams = [];
const transportErrors = [];
let fixtureError = '';
let shouldDropOperation;
let forcedDrops = 0;
let client;
child.stderr.on('data', chunk => { fixtureError += chunk.toString(); });
createInterface({ input: child.stdout }).on('line', line => {
  const value = JSON.parse(line);
  fixtureEvents.push(value);
  events.emit('fixture', value);
});
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function fixture(predicate, ms = 5000) {
  const found = fixtureEvents.find(predicate);
  if (found) return found;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { events.off('fixture', check); reject(new Error(`synthetic fixture timeout; stderr=${fixtureError}`)); }, ms);
    function check(event) { if (predicate(event)) { clearTimeout(timer); events.off('fixture', check); resolve(event); } }
    events.on('fixture', check);
  });
}
function command(value) { child.stdin.write(`${JSON.stringify(value)}\n`); }

try {
  const ready = await fixture(event => event.event === 'ready');
  const endpoint = new URL(ready.url);
  assert.equal(endpoint.hostname, '127.0.0.1');
  const auditedFetch = async (url, init = {}) => {
    const headers = new Headers(init.headers);
    const rpc = typeof init.body === 'string' ? JSON.parse(init.body) : undefined;
    const request = { method: init.method ?? 'GET', rpcMethod: rpc?.method, tool: rpc?.params?.name,
      operation: rpc?.params?.arguments?.operation_id, id: rpc?.id,
      session: headers.get('mcp-session-id'), cursor: headers.get('last-event-id') };
    requests.push(request);
    const response = await fetch(url, init);
    if (!response.body || !response.headers.get('content-type')?.includes('text/event-stream')) return response;
    const upstream = response.body.getReader();
    const decoder = new TextDecoder();
    const capture = { request, text: '', forcedDrop: false };
    streams.push(capture);
    const drop = request.operation === shouldDropOperation && request.method === 'POST';
    const body = new ReadableStream({
      async pull(controller) {
        const chunk = await upstream.read();
        if (chunk.done) { controller.close(); return; }
        capture.text += decoder.decode(chunk.value, { stream: true });
        controller.enqueue(chunk.value);
        if (drop && /id: [^\n]+:0\nretry: \d+\ndata:\n\n/.test(capture.text)) {
          capture.forcedDrop = true;
          forcedDrops++;
          controller.close();
          await upstream.cancel('synthetic dropped original POST after priming');
        }
      },
      cancel(reason) { return upstream.cancel(reason); }
    });
    return new Response(body, { status: response.status, headers: response.headers });
  };
  const transport = new StreamableHTTPClientTransport(endpoint, {
    requestInit: { headers: { Authorization: `Bearer ${ready.token}` } },
    fetch: auditedFetch,
    reconnectionOptions: { initialReconnectionDelay: 100, maxReconnectionDelay: 1000, reconnectionDelayGrowFactor: 1, maxRetries: 3 },
  });
  client = new Client({ name: 'independent-deppy-completion-fixture', version: '1' });
  client.onerror = error => transportErrors.push(error.message);
  await client.connect(transport);
  assert.ok(transport.sessionId, 'official SDK retained initialized host session');

  async function complete(operation, { drop = false, completionDelay = 0, timeout = 5000 } = {}) {
    if (drop) shouldDropOperation = operation;
    const started = performance.now();
    const resultPromise = client.callTool({ name: 'send_text', arguments: {
      session_id: `synthetic-session-${operation}`, generation: 'synthetic-generation',
      operation_id: operation, text: 'synthetic prompt with no PTY side effect', submit: true,
    } }, undefined, { timeout, maxTotalTimeout: timeout, resetTimeoutOnProgress: false });
    const submitted = await fixture(event => event.event === 'submitted' && event.operation_id === operation);
    if (drop) {
      const end = Date.now() + 3000;
      while (!streams.some(stream => stream.request.operation === operation && stream.forcedDrop)) {
        assert.ok(Date.now() < end, 'the original POST was not forcibly disconnected');
        await delay(10);
      }
    }
    await delay(completionDelay);
    command({ complete: operation });
    const result = await resultPromise;
    assert.equal(result.isError, false);
    const payload = JSON.parse(result.content[0].text);
    assert.equal(payload.operation_id, operation);
    assert.equal(payload.event_id, `${submitted.stream_id}:1`);
    assert.equal(payload.local_completion.local_turn_completed, true);
    assert.equal(payload.admission.status, 'queued');
    assert.equal(payload.effect_state, 'queued');
    assert.equal(payload.retry, false);
    assert.equal(payload.retryable, false);
    assert.equal(requests.filter(request => request.tool === 'send_text' && request.operation === operation).length, 1, 'never resubmit input after disconnect');
    const original = requests.find(request => request.tool === 'send_text' && request.operation === operation);
    assert.equal(original.id, submitted.request_id);
    const cursor = `${submitted.stream_id}:0`;
    const finalStream = streams.find(stream => stream.text.includes(`id: ${submitted.stream_id}:1\n`));
    assert.ok(finalStream, 'final SSE was captured on original or original-stream replay');
    const finalRpc = JSON.parse(finalStream.text.split('\n').find(line => line.startsWith('data: {')).slice(6));
    assert.equal(finalRpc.id, original.id, 'server retained original JSON-RPC request ID');
    if (drop) assert.ok(requests.some(request => request.method === 'GET' && request.cursor === cursor && request.session === original.session));
    return { operation, elapsed_ms: Math.round(performance.now() - started), resumed: drop, final_event_id: payload.event_id };
  }
  const outcomes = [];
  const normalTimeout = 500;
  const longDelay = Number(process.env.DEPPY_COMPLETION_LONG_DELAY_MS ?? 750);
  assert.ok(Number.isInteger(longDelay) && longDelay > normalTimeout && longDelay <= 120000);
  outcomes.push(await complete('normal-original-call', { timeout: normalTimeout }));
  outcomes.push(await complete('configured-long-call', { completionDelay: longDelay, timeout: Math.max(3000, longDelay + 3000) }));
  assert.ok(outcomes[1].elapsed_ms >= longDelay);
  outcomes.push(await complete('dropped-original-call', { drop: true, timeout: 5000 }));
  assert.equal(forcedDrops, 1);
  assert.equal(requests.filter(request => request.rpcMethod === 'tools/call' && request.tool !== 'send_text').length, 0, 'zero wait/get/poll tool calls');
  command({ shutdown: true });
  const summary = await fixture(event => event.event === 'summary');
  assert.equal(summary.submitted, 3);
  assert.equal(summary.completed, 3);
  assert.deepEqual(summary.tool_calls, ['send_text', 'send_text', 'send_text']);
  await client.close();
  client = undefined;
  const [exitCode] = await childExit;
  assert.equal(exitCode, 0, fixtureError);
  console.log(JSON.stringify({ status: 'pass', sdk_version: sdkPackage.version,
    sdk_integrity: sdkLock.integrity, package_lock_sha256: createHash('sha256').update(lockText).digest('hex'),
    outcomes, submitted: summary.submitted, completed: summary.completed,
    forced_disconnects: forcedDrops, wait_get_poll_tool_calls: 0,
    resumed_get_requests: requests.filter(request => request.method === 'GET' && request.cursor).length,
    transport_errors: transportErrors,
    configured_normal_timeout_ms: normalTimeout,
    long_completion_delay_ms: longDelay,
    exceeds_sdk_default_60s: longDelay > 60000,
    limitations: ['synthetic transport fixture; no real provider/account or native-turn hook compatibility claim', 'explicit client timeout settings do not establish cloud-host deadline support'] }, null, 2));
} finally {
  if (client) await client.close().catch(() => {});
  if (child.exitCode === null) child.kill('SIGTERM');
}
