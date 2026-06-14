/**
 * brain-client.mjs — thin Node.js client for the Brain Engine daemon.
 *
 * Connects to the Unix domain socket at `<root>/.brain/brain.sock` and
 * exchanges JSON-line messages (one request → one response per connection).
 *
 * Usage (ESM):
 *   import { query, index, status, ping, store } from './brain-client.mjs';
 *
 *   const root = process.cwd();                 // or any initialised project root
 *   const res  = await query(root, 'how does auth work?');
 *   console.log(res.result.chunks);
 *
 * Protocol:
 *   Request  →  {"id":<u32>,"method":"<method>","params":{...}}\n
 *   Response ←  {"id":<u32>,"ok":<bool>,"result":{...}}\n
 *               {"id":<u32>,"ok":false,"error":"<message>"}\n
 */

import net  from 'net';
import path from 'path';

const SOCKET_FILE   = 'brain.sock';
const BRAIN_DIR     = '.brain';
const TIMEOUT_MS    = 30_000;

let _nextId = 1;
function nextId() { return _nextId++; }

/** Absolute path to the daemon socket for a given project root. */
export function socketPath(root) {
  return path.join(root, BRAIN_DIR, SOCKET_FILE);
}

/**
 * Send one request to the daemon and return the parsed response object.
 *
 * @param {string} root     - Project root (directory that contains `.brain/`).
 * @param {string} method   - RPC method name: ping | status | query | index.
 * @param {object} [params] - Optional method parameters.
 * @returns {Promise<{id:number, ok:boolean, result?:object, error?:string}>}
 */
export function request(root, method, params = {}) {
  return new Promise((resolve, reject) => {
    const sock   = socketPath(root);
    const id     = nextId();
    const payload = JSON.stringify({ id, method, params }) + '\n';

    const client = net.createConnection(sock);
    let   buf    = '';
    let   settled = false;

    function settle(fn) {
      if (settled) return;
      settled = true;
      client.destroy();
      fn();
    }

    client.setTimeout(TIMEOUT_MS);

    client.on('connect', () => {
      client.write(payload);
    });

    client.on('data', chunk => {
      buf += chunk.toString('utf8');
      const nl = buf.indexOf('\n');
      if (nl === -1) return;
      const line = buf.slice(0, nl);
      try {
        const parsed = JSON.parse(line);
        settle(() => resolve(parsed));
      } catch (e) {
        settle(() => reject(new Error(`Invalid JSON response: ${line}`)));
      }
    });

    client.on('timeout', () => {
      settle(() => reject(new Error(`Timed out after ${TIMEOUT_MS}ms (${sock})`)));
    });

    client.on('error', err => {
      settle(() => reject(err));
    });

    client.on('end', () => {
      if (!settled) {
        settle(() => reject(new Error('Connection closed before response')));
      }
    });
  });
}

// ---------------------------------------------------------------------------
// Convenience wrappers
// ---------------------------------------------------------------------------

/**
 * Ping the daemon.
 * @param {string} root
 * @returns {Promise<boolean>} true if the daemon responds.
 */
export async function ping(root) {
  try {
    const res = await request(root, 'ping');
    return res.ok === true;
  } catch {
    return false;
  }
}

/**
 * Get daemon status (project name, file/chunk/vector counts, model).
 * @param {string} root
 * @returns {Promise<{ok:boolean, result?:object, error?:string}>}
 */
export function status(root) {
  return request(root, 'status');
}

/**
 * Semantic search: returns the most relevant code chunks for `queryText`.
 *
 * @param {string} root
 * @param {string} queryText
 * @param {object} [opts]
 * @param {number} [opts.topK=5]         - ANN candidates to retrieve.
 * @param {number} [opts.tokens=4000]    - Max context tokens.
 * @param {boolean} [opts.noCache=false] - Bypass cache.
 * @returns {Promise<{ok:boolean, result?:{cache_hit:boolean, chunks:Array, stats:object}, error?:string}>}
 */
export function query(root, queryText, opts = {}) {
  return request(root, 'query', {
    query:    queryText,
    top_k:   opts.topK    ?? 5,
    tokens:  opts.tokens  ?? 4000,
    no_cache: opts.noCache ?? false,
  });
}

/**
 * Trigger an incremental reindex.
 *
 * @param {string} root
 * @param {object} [opts]
 * @param {boolean} [opts.reindex=false]  - Force full reindex.
 * @param {boolean} [opts.noEmbed=false]  - Skip embedding step.
 * @returns {Promise<{ok:boolean, result?:{scanned:number, indexed:number, embedded:number, ...}, error?:string}>}
 */
export function index(root, opts = {}) {
  return request(root, 'index', {
    reindex:  opts.reindex  ?? false,
    no_embed: opts.noEmbed  ?? false,
  });
}

/**
 * Store an assistant response in the cache, keyed by the prompt.
 * @param {string} root
 * @param {string} queryText      - the user prompt
 * @param {string} responseText   - the assistant response to cache
 * @returns {Promise<{ok:boolean, result?:{stored:boolean}, error?:string}>}
 */
export function store(root, queryText, responseText) {
  return request(root, 'store', { query: queryText, response: responseText });
}

// ---------------------------------------------------------------------------
// Hook helpers (called by the generated .claude/hooks/*.sh scripts)
// ---------------------------------------------------------------------------

/** Read all of stdin as a UTF-8 string. */
function readStdin() {
  return new Promise((resolve) => {
    let data = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => { data += c; });
    process.stdin.on('end', () => resolve(data));
    process.stdin.on('error', () => resolve(data));
  });
}

/** Flatten Claude transcript `content` (string | array of blocks) to plain text. */
function extractText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.filter(b => b && b.type === 'text').map(b => b.text).join('');
  }
  return '';
}

/** Format daemon query result into additive context text (or '' if nothing useful). */
function formatContext(result) {
  if (!result) return '';
  if (result.cache_hit && result.response) {
    return `## Brain Engine — cached answer (additive context)\n\n${result.response}\n`;
  }
  const chunks = Array.isArray(result.chunks) ? result.chunks : [];
  if (chunks.length === 0) return '';
  const body = chunks
    .map(c => `### ${c.file_path}:${c.start_line}-${c.end_line}\n\`\`\`\n${c.content.trimEnd()}\n\`\`\``)
    .join('\n\n');
  return `## Brain Engine — retrieved context (additive)\n\n${body}\n`;
}

/**
 * UserPromptSubmit hook entry.
 * Reads the event JSON from stdin, queries the daemon and writes additive
 * context to stdout. Always exits 0; never throws.
 */
export async function hookPrompt() {
  try {
    const raw = await readStdin();
    const evt = JSON.parse(raw);
    const root   = evt.cwd || process.env.BRAIN_ROOT || process.cwd();
    const prompt = typeof evt.prompt === 'string' ? evt.prompt : '';
    if (!prompt.trim()) return;
    if (!(await ping(root))) return;           // daemon down → silent no-op
    const res = await query(root, prompt);
    if (res && res.ok) process.stdout.write(formatContext(res.result));
  } catch {
    /* hooks must never crash the prompt */
  }
}

/**
 * Stop hook entry.
 * Reads the event JSON from stdin, extracts the last user prompt + last
 * assistant message from the JSONL transcript, and stores the pair in cache.
 */
export async function hookStop() {
  try {
    const raw = await readStdin();
    const evt = JSON.parse(raw);
    const root   = evt.cwd || process.env.BRAIN_ROOT || process.cwd();
    const tpath  = evt.transcript_path;
    if (!tpath || !(await ping(root))) return;

    const { readFileSync } = await import('fs');
    const lines = readFileSync(tpath, 'utf8').split('\n').filter(Boolean);
    let lastUser = '', lastAsst = '';
    for (const line of lines) {
      let m;
      try { m = JSON.parse(line); } catch { continue; }
      // Support both `{role, content}` flat entries and `{message: {role, content}}` wrappers.
      const msg  = m.message || m;
      const role = msg.role || m.type;
      const text = extractText(msg.content);
      if (!text) continue;
      if (role === 'user')      lastUser = text;
      else if (role === 'assistant') lastAsst = text;
    }
    if (lastUser && lastAsst) await store(root, lastUser, lastAsst);
  } catch {
    /* swallow */
  }
}

// ---------------------------------------------------------------------------
// CLI helper (run this file directly: node brain-client.mjs <method> [args])
// ---------------------------------------------------------------------------

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const [,, method, ...rest] = process.argv;
  const root = process.env.BRAIN_ROOT ?? process.cwd();

  if (!method) {
    console.error('Usage: node brain-client.mjs <ping|status|query|index|store|hook-prompt|hook-stop> [args...]');
    process.exit(1);
  }

  let result;
  switch (method) {
    case 'ping':
      result = await ping(root);
      console.log(result ? 'pong' : 'no response');
      break;
    case 'status':
      result = await status(root);
      console.log(JSON.stringify(result, null, 2));
      break;
    case 'query':
      if (!rest[0]) { console.error('query requires a text argument'); process.exit(1); }
      result = await query(root, rest.join(' '));
      console.log(JSON.stringify(result, null, 2));
      break;
    case 'index':
      result = await index(root);
      console.log(JSON.stringify(result, null, 2));
      break;
    case 'store':
      if (rest.length < 2) { console.error('store requires <query> <response>'); process.exit(1); }
      result = await store(root, rest[0], rest.slice(1).join(' '));
      console.log(JSON.stringify(result, null, 2));
      break;
    case 'hook-prompt':
      await hookPrompt();
      break;
    case 'hook-stop':
      await hookStop();
      break;
    default:
      console.error(`Unknown method: ${method}`);
      process.exit(1);
  }
}
