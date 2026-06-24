# Brain Engine — Plano de Evolução para "Gestor de Cache de Conhecimento"

> **Para quem vai implementar (Sonnet / GPT-5.5):** este documento é auto-contido.
> Ele descreve o estado atual *real* do código, o gap em relação ao objetivo, e um
> plano por fases com schemas SQL, métodos de daemon, arquivos a tocar e critérios
> de aceite. Implemente fase a fase, na ordem. Cada fase é independente e entrega
> valor sozinha. Rode `cargo test` no fim de cada fase. **Não** quebre os hooks
> existentes (`UserPromptSubmit` / `Stop`) — eles são o ponto de integração.

---

## 0. Objetivo do usuário (fonte da verdade)

> "Quero que esta aplicação seja um gestor de cache. Que armazene toda a informação
> necessária para que, ao usar o Claude Code CLI, ele não precise revisar todo o
> código — simplesmente consulte as informações armazenadas. Deve ser possível
> trocar de sessão e manter o mesmo cache."

Prioridades absolutas (nesta ordem):

1. **Economia de tokens** — nunca mandar o projeto inteiro; mandar conhecimento destilado.
2. **Velocidade / baixa latência** — resposta do hook em < 150 ms no caminho quente.
3. **Eficiência** — alta precisão de contexto com o mínimo de tokens injetados.
4. **Persistência entre sessões** — o cache é o mesmo de uma sessão para outra.
5. **Zero fricção** — funciona invisivelmente via hooks, sem comandos manuais.

---

## 1. O que JÁ existe hoje (estado real do código)

Confirmado por leitura direta do repositório (`v0.10.0`, workspace Rust de 4 crates):

| Capacidade | Onde | Status |
|---|---|---|
| Indexação incremental (walk + chunk + hash) | `brain-core/src/{walk,chunk,hash,index}.rs` | ✅ Funciona |
| Embeddings local (bge/fastembed) + API remota | `brain-embed/src/{local,api,provider}.rs` | ✅ Funciona |
| Vetores em LanceDB | `brain-core/src/vectors.rs` | ✅ Funciona |
| Metadados em SQLite (WAL) | `brain-core/src/db.rs` | ✅ Funciona |
| Retrieval ANN top-k + enriquecimento | `brain-core/src/retrieve.rs` | ✅ Funciona |
| Montagem de contexto por budget de tokens | `brain-core/src/context.rs` | ✅ Funciona |
| **Cache de respostas** SHA-256 exato + semântico (opt-in) + TTL | `brain-core/src/cache.rs` | ✅ Funciona |
| **Persistência entre sessões** (cache em SQLite, lido por toda sessão) | `db.rs` tabela `cache` | ✅ Funciona |
| Hook `UserPromptSubmit` injeta contexto | `.claude/hooks/pre_prompt.sh` → `.brain/brain-client.mjs::hookPrompt` | ✅ Funciona |
| Hook `Stop` salva par (prompt→resposta) no cache | `.claude/hooks/post_response.sh` → `hookStop` | ✅ Funciona |
| Daemon + watcher (reindex incremental em mudança) | `brain-daemon/src/{main,watcher,worker,protocol}.rs` | ✅ Funciona |
| Métricas por request/sessão | `brain-core/src/metrics.rs`, tabelas `requests`/`sessions` | ✅ Funciona |
| Decision engine + model router + fallback DeepSeek | `decision.rs`, `model_router.rs`, `router.rs`, `llm_state.rs` | ✅ Funciona |

### Resposta direta: "ele faz isso atualmente?"

**Parcialmente — sim.** O Brain Engine **já é** um cache persistente entre sessões:
faz RAG (recupera só os trechos relevantes em vez do projeto inteiro) e guarda
pares pergunta→resposta no SQLite, que sobrevivem entre sessões.

**Mas há um gap central** em relação a "não precisar revisar todo o código":

1. **Não existe camada de conhecimento destilado.** A pasta `.brain/summaries/`
   está vazia e o código de sumarização nunca foi implementado (só citada na
   `ARCHITECTURE.md`). Hoje o que se injeta são *chunks brutos de código*, não
   resumos/symbol maps. Para o Claude "consultar em vez de reler", precisamos de
   um **índice estrutural + resumos**.
2. **Sem mapa de símbolos / grafo de código.** Não há tabela de funções, assinaturas,
   imports, definições. Perguntas como "onde está definido X?" ainda exigem leitura.
3. **Cache é só Q→A.** Guarda perguntas já feitas, não uma base de conhecimento do
   código em si. A primeira pergunta sobre qualquer assunto sempre paga retrieval.
4. **Risco de cache obsoleto (staleness).** A chave do cache é `sha256(query+model)`
   com TTL — **não é invalidada quando o código muda**. Uma resposta cacheada pode
   ficar errada após edição dos arquivos que a originaram.
5. **Retrieval só vetorial.** Sem busca híbrida (BM25/keyword) nem reranking; perde
   precisão em queries com nomes exatos de símbolo/arquivo.

As fases abaixo fecham exatamente esses 5 gaps.

---

## 2. Arquitetura-alvo (visão)

```
                    ┌─────────────────────── PROMPT do usuário ──────────────────────┐
                    │                                                                 │
        UserPromptSubmit hook (pre_prompt.sh → brain-client.mjs::hookPrompt)          │
                    │                                                                 │
                    ▼                                                                 │
        ┌───────────────────────── DAEMON (worker.rs::handle_query) ─────────────────┐
        │ 1. Cache Q→A (exato/semântico) ── HIT? ─────────────► devolve resposta      │
        │ 2. MISS:                                                                    │
        │    a. Recupera KNOWLEDGE PACK destilado (NOVO — Fase 1/2):                  │
        │       • resumo do projeto + módulos relevantes                              │
        │       • symbol map dos arquivos relevantes (assinaturas, não corpos)        │
        │    b. Retrieval híbrido vetorial+BM25 + rerank (Fase 4)                     │
        │    c. Monta contexto por budget adaptativo (Fase 3)                         │
        └────────────────────────────────────────────────────────────────────────────┘
                    │
                    ▼ additionalContext (conhecimento destilado, não código bruto)
            Claude responde consultando o pack — relê arquivo só se faltar algo
                    │
        Stop hook (post_response.sh → hookStop): salva Q→A COM fingerprint dos
        arquivos-fonte (Fase 5) para invalidação automática quando o código mudar.
```

### 2.1. Isto aplica automaticamente? (LEIA ANTES DE IMPLEMENTAR)

**Sim, a injeção é automática** — o hook `UserPromptSubmit` roda em **todo prompt**,
sem comando manual. Mas há um **limite técnico que não dá para contornar**:

> Hooks do Claude Code só conseguem **ADICIONAR** contexto (via
> `hookSpecificOutput.additionalContext`). Eles **NÃO** conseguem impedir o Claude de
> usar `Read`/`Grep`/`Glob`. O Claude continua sendo um agente autônomo.

Portanto o objetivo **não** é "o Claude nunca lê arquivo nenhum" (isso é impossível e
nem desejável — para editar, ele precisa ver o código alvo). O objetivo real e
alcançável é **eliminar a fase de descoberta**: o que gasta tokens não é ler o arquivo
certo, é o Claude dar `grep`/`ls`/ler 15 arquivos só para *descobrir onde* mexer.

**Caso de uso alvo (prompt de AÇÃO, não só pergunta):**
`"crie um KPI chamado valor total na aba dashboard"`. O Brain deve injetar, antes do
Claude agir, um **locator + diretiva**:

```
## Brain Engine — locator (para esta tarefa)
Intenção: criar KPI no dashboard.
Arquivos relevantes (NÃO faça grep no projeto — use estes):
  - src/dashboard/KpiGrid.tsx:45   → onde os KPIs são renderizados
  - src/dashboard/types/kpi.ts:12  → tipo `Kpi`
  - src/dashboard/hooks/useKpis.ts → hook de dados
Padrão do projeto: cada KPI é { label, value, format }.
INSTRUÇÃO: edite apenas estes arquivos. Não vasculhe o resto do projeto.
```

Resultado: o Claude lê só os 2-3 arquivos que vai editar e pula a varredura.
**É aí que mora a economia, e vale para todos os prompts.** Isto é a **Fase 0** abaixo.

**Pré-requisito operacional:** cada projeto tem seu próprio `.brain/`. Para o exemplo
do dashboard funcionar, o Brain precisa estar indexado *naquele* projeto
(`brain init && brain index` lá, ou o auto-init do hook na primeira execução).

---

Princípio-chave: **injetar conhecimento destilado (resumos + assinaturas), não
código cru.** Um symbol map de um arquivo de 400 linhas cabe em ~80 tokens; o
arquivo inteiro custa ~1500. É aí que mora a economia.

---

## 3. Plano por fases

### Fase 0 — Intent-Aware Locator + Diretiva de Comportamento ✅ aplicada

**Status de implementação:** aplicado. Existe `brain-core/src/locator.rs`, integrado
ao daemon em `worker.rs::handle_query` e formatado pelo hook em
`clients/node/brain-client.mjs`. A v1 usa classificação heurística e os chunks
recuperados como fonte de alvos; será enriquecida por summaries/symbols nas fases
seguintes.

**Por quê:** é o que faz o sistema servir prompts de **ação** ("crie/edite/adicione X
em Y"), não só perguntas. Sem isto, o contexto injetado são chunks similares à frase —
o que ajuda pouco quando a tarefa é *modificar* algo. Esta fase transforma o prompt em
um **mapa de onde mexer** + uma **instrução para o Claude não vasculhar o projeto**.

> Depende idealmente das Fases 1 (resumos) e 2 (symbol map) para qualidade máxima, mas
> uma **v1 já entrega valor** usando só o retrieval atual + nomes de arquivo/símbolo.
> Recomendo implementar uma v1 cedo e enriquecer depois.

**Implementação** (`brain-core/src/locator.rs` novo + `worker.rs::handle_query`):

1. **Classificação de intenção** (heurística determinística, sem API): detectar verbos
   de ação (criar, adicionar, editar, renomear, remover, corrigir, mover...) e o
   alvo/feature ("KPI", "dashboard", "endpoint", nome de arquivo/símbolo citado no
   prompt). Reusar o `model_router.rs`, que já classifica `type`/`complexity`.
   Saída: `Intent { kind: Action|Question, verb, targets: Vec<String> }`.
2. **Locator:** para uma `Action`, casar `targets` contra:
   - `symbols` (Fase 2) — match por nome de símbolo/feature → arquivo:linha exatos.
   - `summaries` (Fase 1) — match por módulo/feature → arquivos do módulo.
   - retrieval híbrido (Fase 4) — fallback semântico+keyword.
   Devolver uma lista ranqueada de **alvos de edição** `{file, line, why}` (não chunks
   de código brutos), tipicamente 2–5 arquivos.
3. **Injeção da diretiva:** em `brain-client.mjs::hookPrompt`, quando `intent.kind ==
   Action`, formatar o bloco "locator + INSTRUÇÃO" (ver exemplo em §2.1) e colocá-lo
   **no topo** do `additionalContext`, antes do knowledge pack. A instrução textual
   ("edite apenas estes arquivos; não faça grep amplo") é o que orienta o agente a não
   gastar tokens em descoberta — é additive, mas eficaz porque é explícita e específica.
4. Para `Question`, manter o fluxo de knowledge pack normal (Fase 3).

**Config** (`brain.config.json`):
```json
"locator": { "enabled": true, "max_targets": 5, "inject_directive": true }
```

**Critério de aceite:**
- `brain query "crie um KPI valor total na aba dashboard"` (num projeto frontend
  indexado) retorna uma lista de arquivos-alvo com linhas, **não** um dump de chunks.
- O `additionalContext` do hook começa com o bloco locator + a instrução para uma
  query de ação; para uma pergunta, mantém o pack de conhecimento.
- Teste: prompt com verbo de ação ⇒ `Intent::Action`; pergunta ⇒ `Intent::Question`.

**Limite honesto a documentar no README:** o locator *orienta* o Claude; não o *obriga*.
A métrica de sucesso é redução de tool-calls de descoberta (grep/glob/reads amplos),
não zero leituras.

---

### Fase 1 — Camada de Sumarização (Knowledge Digest) ✅ aplicada

**Status de implementação:** aplicado no modo heurístico/local. `MIGRATION_V3`
cria `summaries`; `brain-core/src/summarize.rs` gera resumos de arquivo, módulo e
projeto; `brain index` e `worker.rs::handle_index` sincronizam summaries e escrevem
espelhos em `.brain/summaries/`. `summaries.use_llm` existe na config, mas a rota LLM
opt-in ainda não foi ligada.

**Por quê:** é o coração do "consultar em vez de reler". Resume cada arquivo/módulo/
projeto uma vez (em background) e injeta o resumo, que é 10–20× menor que o código.

**Schema SQL** (adicionar como `MIGRATION_V3` em `brain-core/src/db.rs`):

```sql
CREATE TABLE IF NOT EXISTS summaries (
    id           INTEGER PRIMARY KEY,
    scope        TEXT NOT NULL,        -- 'file' | 'module' | 'project'
    target       TEXT NOT NULL,        -- caminho do arquivo/dir, ou 'PROJECT'
    summary      TEXT NOT NULL,        -- resumo em linguagem natural, curto
    source_hash  TEXT NOT NULL,        -- hash do conteúdo que gerou (p/ invalidar)
    token_estimate INTEGER NOT NULL DEFAULT 0,
    model_used   TEXT,                 -- quem gerou (local heurístico | llm)
    created_at   INTEGER NOT NULL,
    UNIQUE(scope, target)
);
CREATE INDEX IF NOT EXISTS idx_summaries_target ON summaries(target);
```

**Implementação:**

1. Novo módulo `brain-core/src/summarize.rs`:
   - `fn summarize_file(path, content, lang) -> String` — **dois modos**:
     - **Heurístico (default, zero custo):** extrai cabeçalho do arquivo, doc-comments
       de topo, lista de símbolos públicos (reusar o parser da Fase 2), e primeira
       linha de cada função/struct. Determinístico, rápido, sem API.
     - **LLM (opt-in via config):** manda o arquivo (ou seu symbol map) ao provider
       barato (DeepSeek) pedindo um resumo de ≤ 3 frases. Usar o router existente
       (`router.rs`). Só quando `summaries.use_llm = true` em `brain.config.json`.
   - `fn summarize_module(dir, file_summaries) -> String` — agrega resumos de arquivos.
   - `fn summarize_project(module_summaries) -> String` — visão geral do repo.
2. Gerar resumos no fim de cada indexação incremental (`worker.rs::handle_index`),
   **só para arquivos cujo `source_hash` mudou** (reaproveita a lógica de hash já
   existente em `files.hash`). Rodar no daemon, fora do caminho do prompt.
3. Persistir um espelho legível em `.brain/summaries/<path>.md` (a pasta já existe e
   está prevista), útil para inspeção humana.

**Config** (`brain.config.json`, novo bloco):
```json
"summaries": { "enabled": true, "use_llm": false, "max_summary_tokens": 120 }
```

**Critério de aceite:**
- `brain index` popula a tabela `summaries` e `.brain/summaries/`.
- Reindex só regenera resumos de arquivos alterados (verificar via log/contagem).
- `cargo test` cobre: hash inalterado ⇒ não regenera; hash mudou ⇒ regenera.

---

### Fase 2 — Mapa de Símbolos (Code Map) ✅ aplicada

**Status de implementação:** aplicado com extrator heurístico por linhas para
Rust/TypeScript/JavaScript/Python. `MIGRATION_V4` cria `symbols`;
`brain-core/src/symbols.rs` extrai/persiste/consulta símbolos; o indexer substitui
símbolos por arquivo alterado; o daemon expõe `symbols`; o CLI expõe
`brain symbols [name] --kind <kind> --limit <n>`. Tree-sitter segue como melhoria
futura, sem mudar a interface.

**Por quê:** permite responder "onde está X / qual a assinatura de Y / o que este
módulo expõe" sem ler nenhum arquivo. Assinaturas custam uma fração dos corpos.

**Schema SQL** (`MIGRATION_V4`):

```sql
CREATE TABLE IF NOT EXISTS symbols (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,        -- nome do símbolo
    kind        TEXT NOT NULL,        -- 'fn'|'struct'|'enum'|'trait'|'impl'|'const'|'type'|'class'|'method'
    signature   TEXT,                 -- assinatura (sem corpo)
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    visibility  TEXT,                 -- 'pub'|'priv'|null
    doc         TEXT                  -- doc-comment associado, se houver
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
```

**Implementação:**

1. Novo módulo `brain-core/src/symbols.rs`.
2. **Parser:** preferir **tree-sitter** (crates `tree-sitter`, `tree-sitter-rust`,
   `-python`, `-javascript`, `-typescript`, `-go`) para extração robusta. Se quiser
   minimizar peso de build, comece com um extrator por **regex/linha por linguagem**
   (Rust/TS/Python cobrem o uso real aqui) e troque por tree-sitter depois — a
   interface (`fn extract(content, lang) -> Vec<Symbol>`) não muda.
3. Extrair símbolos durante a indexação (`index.rs`), por arquivo, junto com o chunking.
4. Reindex incremental: ao reprocessar um arquivo alterado, `DELETE` símbolos antigos
   daquele `file_id` e reinsere (o `ON DELETE CASCADE` já cobre remoção de arquivo).

**Novo método de daemon** (`protocol.rs` + `worker.rs`): `symbols` —
`{"method":"symbols","params":{"name":"foo","kind":null}}` → lista de
`{file, name, kind, signature, lines}`. Expor também no CLI: `brain symbols <name>`.

**Critério de aceite:**
- Indexar este próprio repo popula `symbols` com as funções públicas de `brain-core`.
- `brain symbols handle_query` retorna `worker.rs:216` com a assinatura.
- Reindex de um arquivo não duplica símbolos.

---

### Fase 3 — Knowledge Pack & Budget Adaptativo no Retrieval 🟠 alta

**Por quê:** unir Fases 1+2 no contexto injetado. Em vez de despejar chunks de código,
montar um "pacote de conhecimento" em camadas, gastando código bruto só no resto do budget.

**Implementação** (`brain-core/src/context.rs` + `worker.rs::handle_query`):

Ordem de preenchimento do budget de tokens (parar quando estourar):
1. **Resumo do projeto** (1 linha, da Fase 1) — sempre.
2. **Resumos dos módulos relevantes** à query (top-N por similaridade do resumo).
3. **Symbol maps** dos arquivos mais relevantes (assinaturas, sem corpo — Fase 2).
4. **Chunks de código bruto** (comportamento atual) — só com o budget restante.

Assim, mesmo budget pequeno já carrega o "mapa mental" do projeto. Tornar os pesos
configuráveis:

```json
"context": { "budget_tokens": 4000, "summary_share": 0.25, "symbol_share": 0.25, "code_share": 0.50 }
```

**Budget adaptativo:** estimar complexidade da query (reusar `model_router.rs`, que já
classifica `complexity` high/low) e escalar `budget_tokens` (query trivial → budget
menor → menos tokens). Logar a decisão em `requests.decision_reason`.

**Critério de aceite:**
- `brain query "como funciona o cache?"` retorna resumo+símbolos+poucos chunks, não
  só chunks crus.
- `context_tokens` medido cai vs. baseline para a mesma pergunta (registrar antes/depois).
- Testes de `assemble()` cobrem as 4 camadas e o corte por budget.

---

### Fase 4 — Retrieval Híbrido + Reranking 🟠 alta

**Por quê:** busca puramente vetorial erra em queries com nomes exatos
(`handle_query`, `cache.rs`). Híbrido vetorial + keyword aumenta precisão ⇒ menos
chunks errados ⇒ menos tokens desperdiçados.

**Implementação:**

1. **BM25 / full-text:** criar tabela virtual FTS5 do SQLite sobre `chunks.content`
   (`MIGRATION_V5`):
   ```sql
   CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
       content, content='chunks', content_rowid='id'
   );
   ```
   Manter sincronizada via triggers `AFTER INSERT/DELETE/UPDATE` em `chunks`.
2. **Fusão:** em `retrieve.rs`, rodar ANN (LanceDB) **e** FTS5, fundir por
   **Reciprocal Rank Fusion** (RRF, `score = Σ 1/(k+rank)`, k=60). Sem dependências novas.
3. **Rerank (opcional, opt-in):** se houver provider de rerank configurado, reordenar
   os top-N fundidos. Default off (mantém latência baixa).

**Critério de aceite:**
- Query com nome exato de função traz o arquivo certo no top-3 (hoje pode falhar).
- FTS fica em sincronia após reindex (teste: insere/edita/remove → conta linhas).

---

### Fase 5 — Invalidação de Cache por Mudança de Código 🔴 prioridade máxima

**Por quê:** corrige o risco de **resposta cacheada obsoleta** após edição. Sem isto,
o "gestor de cache" pode servir informação errada — o pior resultado possível.

**Implementação:**

1. Estender a tabela `cache` (`MIGRATION_V6`):
   ```sql
   ALTER TABLE cache ADD COLUMN source_files TEXT;   -- JSON: ["a.rs","b.rs"]
   ALTER TABLE cache ADD COLUMN source_fingerprint TEXT; -- sha256 dos hashes desses arquivos
   ```
2. No `store` (`cache.rs::store` + `worker.rs::handle_store`): registrar quais arquivos
   alimentaram a resposta (os `file_path` dos chunks recuperados naquele request — já
   disponíveis no `handle_query`) e um fingerprint = `sha256(concat(files.hash))`.
3. No `lookup` (`cache.rs::lookup`): em um hit, **recalcular** o fingerprint atual
   desses arquivos a partir de `files.hash`. Se divergir ⇒ tratar como **MISS** e
   apagar a entrada (lazy invalidation). Garante que cache nunca contradiz o código.
4. O `hookStop` em `brain-client.mjs` já tem o `lastUser`/`lastAsst`; passar também a
   lista de arquivos-fonte do último `query` (cachear no daemon por sessão, ou
   devolver no resultado do `query` e o cliente repassa no `store`).

**Critério de aceite:**
- Editar um arquivo que originou uma resposta cacheada ⇒ próxima pergunta idêntica é
  MISS (recomputa), não serve resposta velha.
- Teste em `cache.rs`: store com fingerprint A, muda `files.hash`, lookup ⇒ Miss.

---

### Fase 6 — Persistência de Contexto de Sessão + Re-hidratação no `/clear` 🔴 prioridade máxima

**Por quê:** este é o pedido central do usuário — "salvar o contexto" e poder começar
uma sessão nova e enxuta usando a memória do brain. Hoje o `hookStop` salva apenas o
último par pergunta→resposta no cache; **não** salva o estado da conversa (decisões,
arquivos tocados, tarefa em andamento). Esta fase salva o contexto de verdade e o
re-injeta numa sessão limpa.

> **DECISÃO DE DESIGN — LEIA (sobre "/clear automático a cada prompt"):**
> O usuário pediu para dar `/clear` automaticamente a cada prompt. **Isto NÃO deve ser
> implementado**, por dois motivos concretos:
> 1. **Inviável tecnicamente:** hooks do Claude Code só *adicionam contexto* ou
>    *bloqueiam um prompt*. Nenhuma saída de hook limpa a sessão. Não há como um hook
>    disparar `/clear`.
> 2. **Contraproducente:** limpar entre cada prompt apaga o estado vivo da tarefa
>    (o que acabou de ser feito/decidido). Em tarefas multi-passo ("crie o KPI" →
>    "agora adicione um teste"), o passo 2 começaria cego e releria tudo → **mais**
>    tokens, tarefa quebrada. O brain re-injeta conhecimento do *código*, não o estado
>    *vivo* da conversa.
>
> **Padrão correto (implementar este):** salvar contexto continuamente + re-hidratar
> num `/clear` **manual/sob demanda** (entre tarefas, não entre prompts), via o hook
> `SessionStart`. Para sessões longas, usar o auto-compact nativo + hook `PreCompact`.

**Implementação:**

1. **Salvar contexto continuamente.** Tabela `session_memory` (`MIGRATION_V7`):
   ```sql
   CREATE TABLE IF NOT EXISTS session_memory (
       id           INTEGER PRIMARY KEY,
       session_id   TEXT,
       kind         TEXT NOT NULL,   -- 'summary'|'decision'|'fact'|'task_state'|'files_touched'
       content      TEXT NOT NULL,
       source_files TEXT,            -- JSON
       created_at   INTEGER NOT NULL
   );
   CREATE INDEX IF NOT EXISTS idx_sessmem_kind ON session_memory(kind);
   ```
   No `hookStop` (`brain-client.mjs`), além de cachear Q→A, extrair do transcript e
   gravar: um **resumo rolante** da sessão, **decisões** ("escolhido JWT em auth.rs"),
   **arquivos tocados** (ler os `tool_use` de Edit/Write no transcript) e o **estado da
   tarefa** (o que falta). Extração heurística por default; LLM barato (DeepSeek via
   `router.rs`) opt-in para resumos melhores.

2. **Re-hidratar na sessão nova.** Instalar um hook `SessionStart` (matchers
   `startup` e `clear`) em `install_hooks.rs` → novo handler
   `brain-client.mjs::hookSessionStart` que injeta um **primer** via
   `additionalContext`:
   ```
   ## Brain Engine — memória da sessão (re-hidratação)
   Projeto: <resumo do projeto, Fase 1>
   Tarefa em aberto: <task_state mais recente>
   Decisões recentes: <últimas N decisions>
   Arquivos relevantes recentes: <files_touched>
   ```
   Assim: o usuário dá `/clear` **quando termina/troca de tarefa**, e a sessão nova
   nasce pequena mas com toda a memória do brain. É exatamente o efeito desejado, sem o
   custo do clear-por-prompt.

3. **Sessões longas sem clear.** Instalar hook `PreCompact` → `hookPreCompact` que
   persiste o resumo rolante **antes** do auto-compact do Claude Code, garantindo que
   nada de importante se perca na compressão.

4. **Comando opcional `brain session save/show`** para inspeção/depuração da memória.

**Critério de aceite:**
- Após uma sessão, `session_memory` contém resumo + decisões + arquivos tocados.
- Dar `/clear` e mandar um prompt relacionado: o Claude já "sabe" da tarefa anterior
  pela injeção do `SessionStart` (verificar no `additionalContext`).
- Confirmado que **nenhum** `/clear` é disparado automaticamente por hook (não existe
  esse mecanismo; documentar no README para alinhar expectativa).

---

### Fase 6b — Cache Global / Multi-Projeto & Portabilidade 🟢 média

**Por quê:** o `brain-plan.md` previa `~/.brain/` global. Hoje só há cache por projeto.
Compartilhar conhecimento estável entre projetos e poder migrar de máquina mantendo o
mesmo cache aumenta a reutilização.

**Implementação:**

1. `~/.brain/` global: `config.json`, `providers.json`, `cache/` (já previsto na
   `ARCHITECTURE`). `paths.rs` já distingue global vs projeto — completar o uso.
2. Fatos duráveis e cross-projeto (libs, padrões) promovidos da `session_memory` local
   para o brain global, com chaves namespaced por projeto (sem conflito, WAL).
3. `brain export` / `brain import`: serializar o conhecimento
   (resumos+símbolos+cache+session_memory) para um arquivo — "trocar de máquina
   mantendo o mesmo cache".

**Critério de aceite:**
- Dois projetos compartilham o cache global sem conflito.
- `brain export && brain import` reconstrói o conhecimento idêntico em outro `.brain/`.

---

### Fase 7 — Velocidade & Observabilidade 🟢 média

**Por quê:** garantir o requisito de latência (< 150 ms no caminho quente) e poder medi-lo.

**Implementação:**

1. **Caminho quente:** no `hookPrompt`, checar cache **antes** de qualquer embedding
   (já é assim para exato — confirmar e garantir ordem). Cache hit deve evitar
   embedding+ANN totalmente.
2. **Warm start do daemon:** manter modelo de embedding e conexões LanceDB/SQLite
   carregados no `WorkerState` (já existe `init_state` — confirmar que o modelo não
   recarrega por request).
3. **Métrica de hit-rate:** adicionar a `metrics.rs`/`brain stats`:
   `cache_hit_rate`, `avg_context_tokens`, `p50/p95 hook latency`, `knowledge_pack_share`.
4. **Bench:** `brain bench` que mede latência do caminho quente e frio sobre N queries.

**Critério de aceite:**
- `brain stats` mostra hit-rate e tokens médios por request.
- p95 do hook < 150 ms em cache hit (medido por `brain bench`).

---

## 4. Ordem de implementação recomendada

```
Fase 0 (locator v1)   ── faz prompts de AÇÃO funcionarem; v1 usa só retrieval atual
Fase 5 (invalidação)  ── corrige risco de resposta obsoleta; pequena
Fase 2 (symbol map)   ── alvos de edição exatos (arquivo:linha) → turbina a Fase 0
Fase 1 (sumarização)  ── maior ganho de economia de tokens
Fase 3 (knowledge pack) ── une 1+2 no contexto injetado
Fase 6 (contexto/sessão) ── salva contexto + re-hidrata no /clear  ← "salvar contexto"
Fase 0 (locator v2)   ── reescreve o locator usando símbolos+resumos  ← objetivo pleno
Fase 4 (retrieval híbrido) ── refina precisão
Fase 6b (global/portabilidade) ── escala multi-projeto
Fase 7 (velocidade/obs) ── valida os SLAs
```

Após **Fases 0+5+2+1+3**, o objetivo central está cumprido: em todo prompt, o Claude
recebe automaticamente *onde mexer* (locator) e *conhecimento destilado* (pack),
consultando o banco em vez de varrer o projeto. O locator (Fase 0) é o que estende
isso de perguntas para **tarefas de ação**, como o exemplo do KPI.

---

## 5. Regras para o implementador

- **Não** mudar a assinatura dos hooks nem o protocolo JSON-line existente; só
  **estender** (`worker.rs` aceita métodos novos; `brain-client.mjs` ganha handlers novos).
- Toda migração SQL é **aditiva e idempotente** (`CREATE … IF NOT EXISTS`,
  `ALTER … ADD COLUMN`). Seguir o padrão `MIGRATION_Vn` de `db.rs` e bump da versão.
- Tudo que custa tempo (sumarizar, parsear símbolos, gerar FTS) roda no **daemon, fora
  do caminho do prompt**. O hook só lê do que já está pronto.
- Honrar `.gitignore`/exclude globs (segredos nunca saem da máquina) — reusar o filtro
  do `walk.rs`. Resumos via LLM só de arquivos permitidos.
- Determinismo por default: modo heurístico sem API liga sozinho; LLM é sempre opt-in.
- `cargo build` e `cargo test` verdes ao fim de cada fase. Adicionar testes por fase.
- Atualizar `ARCHITECTURE.md`, `README.md` e `brain.config.json` de exemplo a cada fase.

## 6. Arquivos-chave (mapa rápido para tocar)

| Precisa de | Arquivo |
|---|---|
| Schema SQL / migrações | `crates/brain-core/src/db.rs` |
| Indexação (gancho p/ resumos e símbolos) | `crates/brain-core/src/index.rs` |
| Walk / filtros de arquivo | `crates/brain-core/src/walk.rs` |
| Chunking | `crates/brain-core/src/chunk.rs` |
| Retrieval (híbrido + RRF) | `crates/brain-core/src/retrieve.rs` |
| Montagem de contexto (knowledge pack) | `crates/brain-core/src/context.rs` |
| Cache (invalidação por fingerprint) | `crates/brain-core/src/cache.rs` |
| Router de modelo (complexidade/budget) | `crates/brain-core/src/model_router.rs` |
| Métricas / stats | `crates/brain-core/src/metrics.rs` |
| Daemon: handlers de método | `crates/brain-daemon/src/worker.rs` |
| Daemon: protocolo JSON-line | `crates/brain-daemon/src/protocol.rs` |
| Watcher (reindex incremental) | `crates/brain-daemon/src/watcher.rs` |
| Cliente Node dos hooks | `.brain/brain-client.mjs` |
| Hooks shell | `.claude/hooks/{pre_prompt,post_response}.sh` |
| Comandos CLI (expor `symbols`, `bench`, etc.) | `crates/brain-cli/src/commands/*.rs` |
| Config de exemplo | `brain.config.json` |

## 7. Novos módulos a criar

- `crates/brain-core/src/summarize.rs` (Fase 1)
- `crates/brain-core/src/symbols.rs` (Fase 2)
- `crates/brain-cli/src/commands/symbols.rs` e `.../bench.rs` (Fases 2 e 7)
- Migrações `MIGRATION_V3..V6` em `db.rs` (Fases 1, 2, 4, 5)
