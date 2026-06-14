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
import os   from 'os';
import fs   from 'fs';

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

/**
 * Detect whether a Claude API error indicates the token quota is exhausted.
 * Checks both structured error types and plain-text error messages.
 * @param {object} evt   - The Stop hook event JSON.
 * @param {string} lastAsst - Last assistant message text from the transcript.
 * @returns {boolean}
 */
function isRateLimitError(evt, lastAsst) {
  // 1. The Stop event may carry a stop_reason of 'error' with an error_type.
  const errorType = evt.error_type || evt.error?.type || '';
  const rateLimitTypes = ['rate_limit_error', 'overloaded_error', 'usage_limit_exceeded'];
  if (rateLimitTypes.includes(errorType)) return true;

  // 2. Scan the last assistant message for known error strings.
  const patterns = [
    /rate.?limit/i,
    /usage.?limit/i,
    /token.?quota/i,
    /too many requests/i,
    /overloaded/i,
    /claude.*unavailable/i,
    /\b529\b/,            // HTTP 529 (Anthropic overload)
    /\b429\b/,            // HTTP 429 (rate limit)
  ];
  return patterns.some(re => re.test(lastAsst));
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

// ---------------------------------------------------------------------------
// Eco-mode content compressor (caveman-inspired: drop fluff, keep substance)
// ---------------------------------------------------------------------------

// Patterns for lines that are purely decorative — never carry code semantics.
// Matches: `// ====`, `# ---`, `/* ===`, ` * ===`, blank dividers, etc.
const DECORATIVE_LINE_RE = /^[\s]*(?:\/\/|#|\/\*|\*)[\s=\-*~_]{4,}[\s]*(?:\*\/)?[\s]*$/;

// Matches lines that are *only* whitespace after stripping indent.
const BLANK_RE = /^\s*$/;

/**
 * Compress a code chunk's content using caveman-style rules:
 *   1. Strip trailing whitespace from every line (always safe)
 *   2. Remove purely decorative comment dividers (// ===, # ---, etc.)
 *   3. Collapse 3+ consecutive blank lines into one
 *
 * Never alters functional code — only structural/visual fluff.
 *
 * @param {string} src    - Raw chunk content
 * @param {boolean} ultra - When true, also strip leading import-only blocks
 *                          and collapse runs of single-line doc-comments
 * @returns {string}
 */
function compressCode(src, ultra = false) {
  let lines = src.split('\n');

  // 1. Strip trailing whitespace
  lines = lines.map(l => l.trimEnd());

  // 2. Drop decorative divider lines
  lines = lines.filter(l => !DECORATIVE_LINE_RE.test(l));

  // 3. Collapse 3+ consecutive blank lines → 1 blank line (caveman: "fragments OK")
  const collapsed = [];
  let blankRun = 0;
  for (const l of lines) {
    if (BLANK_RE.test(l)) {
      blankRun++;
      if (blankRun <= 2) collapsed.push(l);
    } else {
      blankRun = 0;
      collapsed.push(l);
    }
  }
  lines = collapsed;

  if (ultra) {
    // Ultra: strip leading import-only blocks (top of file boilerplate)
    // Find where the first non-import, non-blank line is.
    const IMPORT_RE = /^[\s]*(import|from|require|use |#include|using |package )/;
    let firstNonImport = 0;
    while (firstNonImport < lines.length &&
           (BLANK_RE.test(lines[firstNonImport]) || IMPORT_RE.test(lines[firstNonImport]))) {
      firstNonImport++;
    }
    // Only strip if the import block is not the entire chunk and occupies >3 lines.
    if (firstNonImport > 3 && firstNonImport < lines.length) {
      lines = [`// [${firstNonImport} import lines omitted]`, ...lines.slice(firstNonImport)];
    }
  }

  return lines.join('\n').trimEnd();
}

/**
 * Eco-mode context formatter — compact plain-text, no fenced blocks.
 * Applies caveman compression to each chunk: drops decorative dividers,
 * collapses blank lines, strips trailing whitespace.
 *
 * output_style "eco"       → standard compression
 * output_style "eco-ultra" → also strips import blocks
 */
function formatContextEco(result, ultra = false) {
  if (!result) return '';
  if (result.cache_hit && result.response) {
    return `[brain:cache] ${result.response.trim()}\n`;
  }
  const chunks = Array.isArray(result.chunks) ? result.chunks : [];
  if (chunks.length === 0) return '';
  const body = chunks
    .map(c => {
      const header = `[${c.file_path}:${c.start_line}-${c.end_line}]`;
      const compressed = compressCode(c.content, ultra);
      const lines = compressed.split('\n').map(l => `  ${l}`).join('\n');
      return `${header}\n${lines}`;
    })
    .join('\n');
  return `[brain]\n${body}\n`;
}

// ---------------------------------------------------------------------------
// Metrics panel
// ---------------------------------------------------------------------------

/** Compact token formatter: 95844 → "95.8k", 320 → "320". */
function fmtTokens(n) {
  if (n == null || Number.isNaN(n)) return '?';
  if (Math.abs(n) >= 1000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
  return String(n);
}

// ANSI colors for the user-facing panel. Honors NO_COLOR (https://no-color.org).
const COLOR_ENABLED = !process.env.NO_COLOR;
const C = {
  reset:  '\x1b[0m',
  dim:    '\x1b[2m',
  brain:  '\x1b[1m\x1b[38;5;213m', // bold pink — the 🧠 label
  label:  '\x1b[38;5;245m',        // grey — field labels
  cost:   '\x1b[1m\x1b[38;5;208m', // bold orange — real cost (the metric that matters)
  good:   '\x1b[38;5;42m',         // green — reduction / efficiency
  info:   '\x1b[38;5;75m',         // blue — neutral values
  warn:   '\x1b[38;5;220m',        // yellow — cache miss / attention
};
/** Wrap `s` in color `code` when colors are enabled, else return it plain. */
function paint(code, s) {
  return COLOR_ENABLED ? `${code}${s}${C.reset}` : String(s);
}

/** Read brain.config.json (best-effort), returns {} on any error. */
function readProjectConfig(root) {
  try {
    return JSON.parse(fs.readFileSync(path.join(root, 'brain.config.json'), 'utf8'));
  } catch {
    return {};
  }
}

/** Read the configured embedding provider for the Mode label (best-effort). */
function embeddingProvider(root) {
  return readProjectConfig(root).embedding_provider || 'local';
}

/**
 * Returns the brain context-injection style for this project.
 * This governs how brain compresses the *retrieved context* it injects —
 * separate from the Claude Code response output style (which lives in
 * .claude/output-styles/ and is selected via /config).
 *
 * Value comes from `output_style` in brain.config.json:
 *   "rich" (default) | "eco" | "eco-ultra"
 */
function outputStyle(root) {
  return readProjectConfig(root).output_style || 'rich';
}

/**
 * Minimal one-line metrics for eco mode — no field labels, no color variants.
 * Example: `🧠 12ms · +2.1k · 3 chunks · CACHE:HIT`
 */
function ecoMetricsPanel(result, elapsedMs) {
  const r = result || {};
  const s = r.stats || {};
  const chunks = Array.isArray(r.chunks) ? r.chunks.length : 0;
  const cost   = s.real_cost != null ? `+${fmtTokens(s.real_cost)}` : '';
  const cache  = r.cache_hit ? 'HIT' : 'MISS';
  const parts  = [`${elapsedMs}ms`, cost, `${chunks}ch`, `C:${cache}`].filter(Boolean);
  return `🧠 ${parts.join(' · ')}`;
}

/** Current system CPU load (%) and used RAM (MB), best-effort. */
function systemLoad() {
  let cpu = 0;
  try {
    const cores = os.cpus().length || 1;
    cpu = Math.min(100, Math.max(0, Math.round((os.loadavg()[0] / cores) * 100)));
  } catch { /* ignore */ }
  let ramMb = 0;
  try { ramMb = Math.round((os.totalmem() - os.freemem()) / 1048576); } catch { /* ignore */ }
  return { cpu, ramMb };
}

/**
 * Build the per-request `[Brain Metrics]` panel from a query result.
 *
 * Honest metrics (see context.rs): we headline the **real cost** added to the
 * prompt, plus the (non-accumulated, informative) theoretical reduction and the
 * efficiency ratio. The old inflated "Saved" headline is gone.
 *
 * @param {object}  result            - daemon query result (chunks, stats, cache_hit).
 * @param {number}  elapsedMs         - round-trip time measured client-side.
 * @param {string}  root              - project root (for the Mode label).
 * @param {object}  [opts]
 * @param {boolean} [opts.color=false]- emit ANSI colors (only for the user panel).
 */
function metricsPanel(result, elapsedMs, root, { color = false } = {}) {
  const r = result || {};
  const s = r.stats || {};
  const chunks = Array.isArray(r.chunks) ? r.chunks.length : 0;
  const mode   = r.cache_hit ? 'CACHE' : embeddingProvider(root).toUpperCase();
  const { cpu, ramMb } = systemLoad();
  // `paint` is a no-op when color is off → identical plain text for additionalContext.
  const p = color ? paint : (_c, v) => String(v);
  const eff = s.efficiency_ratio != null ? (s.efficiency_ratio * 100) : null;

  const parts = [
    `${p(C.label, 'Time:')} ${elapsedMs}ms`,
    `${p(C.label, 'Context:')} ${p(C.info, fmtTokens(s.context_tokens) + ' tok')}`,
    // The metric that actually matters: tokens added to the prompt.
    s.real_cost != null
      ? `${p(C.label, 'Cost:')} ${p(C.cost, '+' + fmtTokens(s.real_cost))}`
      : null,
    s.reduction_pct != null
      ? `${p(C.label, 'Reduction:')} ${p(C.good, s.reduction_pct + '%')}`
      : null,
    eff != null
      ? `${p(C.label, 'Eff:')} ${p(C.good, eff.toFixed(1) + '%')}`
      : null,
    `${p(C.label, 'Chunks:')} ${chunks}`,
    `${p(C.label, 'Mode:')} ${p(C.info, mode)}`,
    `${p(C.label, 'Cache:')} ${r.cache_hit ? p(C.good, 'HIT') : p(C.warn, 'MISS')}`,
    `${p(C.label, 'CPU:')} ${cpu}%`,
    `${p(C.label, 'RAM:')} ${ramMb}MB`,
  ].filter(Boolean);
  return `${p(C.brain, '🧠 [Brain Metrics]')} ${parts.join(' · ')}`;
}

/**
 * Build the `[MODEL ROUTER]` line from a query result's `model_router` object.
 *
 * The Rust daemon classifies the prompt (type/complexity/critical) and selects
 * a model tier by score; we surface that decision inline so the user can see
 * which model the request *should* go to. Returns null when routing is
 * disabled (model_router absent/null) so nothing is shown.
 *
 * @param {object}  result            - daemon query result.
 * @param {object}  [opts]
 * @param {boolean} [opts.color=false]- emit ANSI colors.
 */
function modelRouterPanel(result, { color = false } = {}) {
  const mr = result && result.model_router;
  if (!mr || !mr.selected_model) return null;
  const cls = mr.classification || {};
  const p = color ? paint : (_c, v) => String(v);
  const parts = [
    `${p(C.label, 'Type:')} ${p(C.info, cls.type ?? '?')}`,
    `${p(C.label, 'Complexity:')} ${p(C.info, cls.complexity ?? '?')}`,
    `${p(C.label, 'Critical:')} ${cls.is_critical ? p(C.warn, 'yes') : 'no'}`,
    `${p(C.label, 'Model:')} ${p(C.cost, String(mr.selected_model).toUpperCase())}`,
  ];
  return `${p(C.brain, '🧭 [MODEL ROUTER]')} ${parts.join(' · ')}`;
}

/**
 * Build the aggregated session panel by reading today's request log.
 * Returns null if there is nothing to report.
 */
function sessionPanel(root) {
  try {
    const day  = new Date().toISOString().slice(0, 10);
    const file = path.join(root, '.brain', 'logs', `${day}.log`);
    const lines = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean);
    // Accumulate the REAL injected cost (context_tokens), never the theoretical
    // saved figure — that baseline is fixed and summing it is meaningless.
    let n = 0, realTokens = 0, time = 0, hits = 0;
    for (const l of lines) {
      let m; try { m = JSON.parse(l); } catch { continue; }
      n++;
      realTokens += m.context_tokens_estimated || 0;
      time       += m.response_time_ms || 0;
      if (m.cache_hit) hits++;
    }
    if (!n) return null;
    const avgTok = Math.round(realTokens / n);
    const parts = [
      `${paint(C.label, 'Requests:')} ${n}`,
      `${paint(C.label, 'Total context injected:')} ${paint(C.info, '~' + fmtTokens(realTokens) + ' tok')}`,
      `${paint(C.label, 'Avg per request:')} ${paint(C.cost, '~' + fmtTokens(avgTok) + ' tok')}`,
      `${paint(C.label, 'Cache:')} ${paint(C.good, hits + '/' + n)} hits`,
    ];
    return `${paint(C.brain, '🧠 [Brain Metrics · session]')} ${parts.join(' · ')}`;
  } catch {
    return null;
  }
}

/**
 * UserPromptSubmit hook entry.
 * Queries the daemon, then emits a JSON payload so the user sees the
 * `[Brain Metrics]` panel inline (via `systemMessage`) while Claude receives
 * the retrieved chunks (via `hookSpecificOutput.additionalContext`).
 * Always exits 0; never throws.
 */
export async function hookPrompt() {
  try {
    const raw = await readStdin();
    const evt = JSON.parse(raw);
    const root   = evt.cwd || process.env.BRAIN_ROOT || process.cwd();
    const prompt = typeof evt.prompt === 'string' ? evt.prompt : '';
    if (!prompt.trim()) return;
    if (!(await ping(root))) return;           // daemon down → silent no-op

    const style = outputStyle(root);
    const eco   = style === 'eco' || style === 'eco-ultra';
    const ultra = style === 'eco-ultra';
    const t0    = Date.now();
    // Eco modes fetch fewer chunks and a smaller token budget to reduce injection cost.
    const res = await query(root, prompt, eco ? { topK: 3, tokens: 2000 } : {});
    const elapsedMs = Date.now() - t0;
    if (!res || !res.ok) return;

    if (eco) {
      // Eco: single-line metrics, caveman-compressed context, no model-router line.
      const panel = ecoMetricsPanel(res.result, elapsedMs);
      const ctx   = formatContextEco(res.result, ultra);
      const additionalContext = ctx ? `${panel}\n\n${ctx}` : `${panel}\n`;
      process.stdout.write(JSON.stringify({
        systemMessage: panel,
        hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext },
      }));
      return;
    }

    // Rich (default): colored panel, fenced-code context, model-router line.
    const panelColor = metricsPanel(res.result, elapsedMs, root, { color: true });
    const panelPlain = metricsPanel(res.result, elapsedMs, root, { color: false });
    const routerColor = modelRouterPanel(res.result, { color: true });
    const routerPlain = modelRouterPanel(res.result, { color: false });
    const ctx   = formatContext(res.result);
    // Belt-and-suspenders: the panel goes into `systemMessage` (rendered to the
    // user inline) AND is prepended to `additionalContext` (so it survives in
    // the transcript / Claude's view even if systemMessage is suppressed).
    const userPanel  = routerColor ? `${panelColor}\n${routerColor}` : panelColor;
    const plainPanel = routerPlain ? `${panelPlain}\n${routerPlain}` : panelPlain;
    const additionalContext = ctx ? `${plainPanel}\n\n${ctx}` : `${plainPanel}\n`;
    process.stdout.write(JSON.stringify({
      systemMessage: userPanel,
      hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext },
    }));
  } catch {
    /* hooks must never crash the prompt */
  }
}

/**
 * Stop hook entry.
 * Reads the event JSON from stdin, extracts the last user prompt + last
 * assistant message from the JSONL transcript, and stores the pair in cache.
 * Also detects Claude rate-limit errors and calls `brain llm block claude`
 * so future requests are automatically routed to DeepSeek.
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

    // Detect Claude rate-limit / quota-exhausted errors and block Claude
    // automatically so the router switches to DeepSeek on the next request.
    if (isRateLimitError(evt, lastAsst)) {
      try {
        const { execFileSync } = await import('child_process');
        execFileSync('brain', ['--path', root, 'llm', 'block', 'claude'], {
          timeout: 5_000,
          stdio:   'pipe',
        });
        process.stderr.write('🧠 brain  Claude rate-limited → switching to DeepSeek\n');
      } catch {
        // If `brain` is not in PATH, fall back to a no-op — the user will see
        // DeepSeek routing once they manually run `brain llm block claude`.
      }
    }

    if (lastUser && lastAsst) {
      await store(root, lastUser, lastAsst);
      process.stderr.write('🧠 brain  cached\n');
    }

    // Stop hooks cannot print to the user inline via stdout/stderr, but the
    // `systemMessage` JSON field is rendered. Surface the session aggregate so
    // the user sees Brain Engine metrics *after* each completed request.
    const panel = sessionPanel(root);
    const out = { suppressOutput: true };
    if (panel) out.systemMessage = panel;
    process.stdout.write(JSON.stringify(out));
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
