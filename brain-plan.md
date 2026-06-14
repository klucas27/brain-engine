# SYSTEM PROMPT — BRAIN ENGINE PARA CLAUDE CODE (AUTÔNOMO, HÍBRIDO, MULTI-PROJETO)

Você deve atuar como um arquiteto sênior de sistemas e implementar um sistema completo chamado:

> **Brain Engine (Local AI Context Layer)**

Este sistema será integrado ao Claude Code CLI de forma transparente e automática, sem exigir comandos adicionais do usuário.

---

# 🎯 OBJETIVO PRINCIPAL

Criar uma camada de inteligência local que:

* Elimina a necessidade de enviar o projeto inteiro ao Claude
* Reduz drasticamente o uso de tokens
* Minimiza alucinações
* Aumenta velocidade e precisão
* Funciona automaticamente em qualquer projeto
* Suporta múltiplas sessões simultâneas
* Decide dinamicamente entre processamento local e API

---

# 🧠 ARQUITETURA GERAL

Você deve implementar:

### 1. Cérebro Global (Global Brain)

Local: `~/.brain/`

Responsável por:

* Cache global de respostas
* Memória compartilhada entre projetos
* Configuração de provedores (APIs e modelos locais)
* Perfis de uso
* Histórico semântico

---

### 2. Cérebro por Projeto (Project Brain)

Local: `<project_root>/.brain/`

Responsável por:

* Indexação do código
* Embeddings vetoriais
* Contexto específico do projeto
* Resumos estruturais
* Cache local

---

# ⚙️ FUNCIONAMENTO AUTOMÁTICO (OBRIGATÓRIO)

Ao iniciar o Claude Code em qualquer projeto:

1. Detectar se `.brain/` existe
2. Se não existir:

   * Criar estrutura completa automaticamente
   * Rodar indexação inicial (full scan)
3. Iniciar watcher de arquivos
4. Ativar hooks automaticamente
5. Operar de forma invisível (sem intervenção do usuário)

---

# 📦 ESTRUTURA DE DIRETÓRIOS

## Global

```
~/.brain/
  config.json
  providers.json
  cache/
  memory/
  logs/
```

## Projeto

```
.project_root/
  .brain/
    vectors/
    cache/
    summaries/
    metadata.db
    embeddings.db
  .claude/
    hooks/
      pre_prompt.sh
      post_response.sh
  brain.config.json
```

---

# 🧠 BANCO DE DADOS

## Obrigatório (híbrido)

### Vetorial:

* LanceDB (preferencial)
* fallback: Chroma

### Relacional:

* SQLite

---

# ⚡ ENGINE EM RUST (OBRIGATÓRIO)

Você deve criar um serviço em Rust para:

* Indexação paralela (Rayon)
* Processamento de arquivos
* Chunking inteligente
* Atualização incremental
* Busca vetorial rápida

---

# ⚡ PARALELISMO

Use paralelismo agressivo:

* Indexação multi-core
* Processamento em batch
* Atualizações concorrentes

---

# 🧠 EMBEDDINGS (HÍBRIDO INTELIGENTE)

O sistema deve decidir automaticamente entre:

## Local

* Modelos leves (ex: bge-small)
* CPU only

## API

* DeepSeek (default)
* ou qualquer outro provider configurado

---

# 🧠 DECISÃO DINÂMICA (CRÍTICO)

Implementar um sistema que avalia:

* Uso de CPU
* Uso de memória
* Tempo estimado
* Custo de API

### Regras:

Se:

* CPU alta OU RAM alta → usar API
* Batch grande → usar local
* Query crítica → usar API
* Baixa carga → usar local

---

# 🔄 ROUTER DE IA

Criar um roteador que decide:

* DeepSeek → leitura, embeddings, análise simples
* Claude → raciocínio complexo e geração

---

# 🔍 RETRIEVAL INTELIGENTE

Antes de cada prompt:

1. Gerar embedding da pergunta
2. Buscar top-k chunks relevantes
3. Injetar no prompt

NUNCA enviar o projeto inteiro

---

# 🧩 HOOKS DO CLAUDE (OBRIGATÓRIO)

Criar:

### pre_prompt.sh

* Intercepta prompt
* Injeta contexto relevante

### post_response.sh

* Armazena resposta
* Atualiza cache/memória

---

# 💾 CACHE INTELIGENTE

Implementar:

* Cache por hash (SHA256)
* TTL configurável
* Cache semântico

---

# 👁 WATCHER DE ARQUIVOS

* Detectar mudanças em tempo real
* Reindexar apenas arquivos alterados

---

# 🧠 SUMMARIZATION LAYER

Criar resumos automáticos:

* Por pasta
* Por módulo
* Por serviço

---

# 🔌 EXTENSIBILIDADE (CRÍTICO)

O sistema NÃO deve ser dependente de DeepSeek

Criar suporte a múltiplos providers:

```
providers.json
```

Exemplo:

```
{
  "embedding": ["local", "deepseek", "openai"],
  "llm": ["claude", "deepseek"]
}
```

---

# 🧠 MULTI-SESSÃO

* Cada projeto isolado
* Compartilhamento via global brain
* Sem conflitos

---

# 🚀 CLI INTERNO

Criar comandos internos:

* `brain init`
* `brain index`
* `brain query`
* `brain status`

(embora o sistema funcione automaticamente)

---

# 📊 LOGGING E MÉTRICAS (CRÍTICO)

O sistema deve registrar métricas em dois níveis:

🔹 1. Por REQUEST (obrigatório)

Registrar a cada execução:

response_time_ms
context_tokens_estimated
tokens_saved_estimated
chunks_used
retrieval_time_ms
embedding_source (local | api)
llm_used (claude | outro)
cpu_usage_percent
memory_usage_mb
decision_reason (ex: "high_cpu → api")

🔹 2. Por SESSÃO (agregado)

Ao longo da sessão:

total_requests
avg_response_time
total_tokens_saved_estimated
api_calls_count
local_calls_count
cache_hits
cache_miss

🔹 3. Persistência

Salvar em:

.brain/logs/YYYY-MM-DD.log

Formato JSON:

{
  "timestamp": 171823123,
  "response_time_ms": 320,
  "tokens_saved_estimated": 12500,
  "cpu": 62,
  "memory": 480,
  "decision": "local_embedding"
}

🔹 4. Exibição no CLI (importante)

Exibir no final de CADA REQUEST (não só sessão):

[Brain Metrics]
Time: 320ms
Context: 2.1k tokens
Saved: ~12.5k tokens
CPU: 62% | RAM: 480MB
Mode: LOCAL
Cache: HIT

🔹 5. Consolidação opcional

Criar comando:

brain stats

Para visualizar:

economia total
performance média
uso de APIs vs local

🎯 OBJETIVO

Essas métricas devem servir para:

otimizar decisões automaticamente
reduzir custo progressivamente
ajustar heurísticas do sistema

---

# 🎯 FOCO PRINCIPAL

Prioridades absolutas:

1. Redução de tokens
2. Baixa latência
3. Zero fricção para o usuário
4. Alta precisão de contexto
5. Escalabilidade multi-projeto

---

# 🚫 REGRAS IMPORTANTES

* Nunca enviar projeto inteiro ao Claude
* Sempre usar retrieval
* Sempre usar cache
* Sempre tentar reduzir custo

---

# 📦 TECNOLOGIAS

* Rust (core engine)
* Node.js (CLI wrapper)
* SQLite
* LanceDB
* Shell scripts (hooks)

---

# ✅ RESULTADO FINAL ESPERADO

Um sistema totalmente funcional onde:

* O usuário apenas usa o Claude Code normalmente
* O “cérebro” funciona invisivelmente
* O contexto é sempre otimizado
* O custo é drasticamente reduzido
* A performance é significativamente maior

---

# EXECUÇÃO

Implemente todo o sistema completo:

* Código
* Estrutura
* Scripts
* Integrações
* Configurações

Sem simplificações.

O sistema deve estar pronto para uso real em produção local.
