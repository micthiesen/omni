// The kitchen agent's side of a request, for the browser tests.
//
// On the mock server it is played through /__mock/*. Against a real Omni
// (FOND_AGENT=claude) each step is one MCP tool call made by a fresh
// `claude -p` client, which is how a deployed tool is reached from a machine
// whose running sessions connected before the deploy.
const { execFileSync } = require('child_process');
const os = require('os');

const REAL = process.env.FOND_AGENT === 'claude';

function tool(name, input) {
  const prompt =
    `Call the tool mcp__omni__${name} exactly once with exactly this JSON input: ${JSON.stringify(input)}\n` +
    'Then reply with only the JSON object the tool returned (or {"error": "<the error text>"} if it failed). ' +
    'No prose and no code fence.';
  const out = execFileSync(
    'claude',
    ['-p', prompt, '--model', 'haiku', '--allowedTools', `mcp__omni__${name}`, '--output-format', 'json'],
    { cwd: os.tmpdir(), encoding: 'utf8', timeout: 180_000, maxBuffer: 16 * 1024 * 1024 },
  );
  const text = String(JSON.parse(out).result || '');
  const body = text.slice(text.indexOf('{'), text.lastIndexOf('}') + 1);
  const result = JSON.parse(body);
  if (result.error) throw new Error(`${name}: ${result.error}`);
  return result;
}

// Every request with `status` ("all" for any), oldest first:
// { id, text, status, origin, recipeId, revision, scale, step }.
async function requests(request, status = 'queued') {
  if (REAL) {
    const found = [];
    for (let cursor = 0; cursor !== undefined; ) {
      const page = tool('recipes_requests_list', { status, limit: 100, cursor });
      found.push(...page.requests);
      cursor = page.nextCursor;
    }
    return found.map((r) => ({ ...r, id: r.requestId }));
  }
  const answer = await request.post('/__mock/requests', { data: { status } });
  return (await answer.json()).requests.map((r) => ({ ...r, ...r.context }));
}

// recipes_request_update: { requestId, status?, note?, resultRevision?, resultRecipeId? }.
async function update(request, change) {
  if (REAL) return tool('recipes_request_update', change).request;
  const answer = await request.post('/__mock/request', { data: change });
  if (!answer.ok()) throw new Error(await answer.text());
  return answer.json();
}

// Writes a new revision of a recipe whose summary is `summary`, and returns
// the revision number.
async function revise(request, recipeId, summary) {
  if (REAL) {
    const head = tool('recipes_get', { id: recipeId, view: 'text' }).headRevision;
    return tool('recipes_edit', {
      id: recipeId,
      expectedRevision: head,
      summary: 'Browser test: summary',
      ops: [{ op: 'setRecipe', fields: { summary } }],
    }).revision;
  }
  const answer = await request.post('/__mock/revise', { data: { recipeId, summary } });
  return (await answer.json()).revision;
}

module.exports = { REAL, requests, update, revise };
