# 0001 — Keep llama.cpp as the managed local default; keep other providers external

[Decision index](README.md) · [Documentation home](../README.md) ·
[Configuration](../CONFIGURATION.md) · [V2 architecture](../v2/ARCHITECTURE.md)

- **Status:** Accepted for Cartograph v2
- **Last reviewed:** 2026-10-04
- **Scope:** Optional embedding, reranking, summary, classification, ask,
  local-chat, and decision (Jev) providers.

## Context

Cartograph's structural engine must remain useful without any model. Optional
model tiers need a bounded HTTP/process boundary that works across platforms
without adding a Python, Node, or provider SDK runtime to the native release.

MLX can be attractive on Apple Silicon, Ollama simplifies model lifecycle, and
cloud/OpenAI-compatible or Anthropic services provide larger models. Their
installation, model compatibility, and process ownership differ. In
particular, an embedding-model change creates a distinct vector model identity
and requires embedding the current generation; vectors are never silently
reused across models.

## Decision

- The release archive contains only Cartograph's native Rust executable and
  allowlisted notices. It does not bundle llama.cpp, MLX, Python, model files,
  or provider SDKs.
- `llama-server` remains the only local process family that `cartograph
  backend` may explicitly start/stop/inspect. It is installed separately and
  configured with absolute model paths and loopback endpoints.
- MLX, Ollama, LM Studio, vLLM, LocalAI, and other OpenAI-compatible servers are
  supported as externally managed endpoints.
- Each tier names one provider:

  | Provider | Tiers | Boundary |
  | --- | --- | --- |
  | `openai-compat` | All chat tiers, embedding, and reranker | OpenAI-compatible HTTP, including the external endpoints above |
  | `anthropic-api` | Chat tiers | Anthropic Messages API |
  | `claude-bridge` | Chat tiers | Bounded local Claude CLI bridge |
  | `cli-bridge` | Chat tiers | Generic bounded local CLI bridge: an operator-chosen executable with a bounded argv template, never a shell |
  | `typesafe` | Decision tier only | Typesafe's typed, parallel Jev decision API for bounded retrieval planning |

  Embedding/reranker tiers remain OpenAI-compatible HTTP, and the decision tier
  accepts only `typesafe`.
- Credentials resolve from a named environment variable (`apiKeyEnv`) or a
  shell-free credential command (`apiKeyCommand`). The bridge providers take no
  credential. Legacy inline keys are readable only for migration compatibility
  and are never returned.
- Structural summaries, exact/BM25 retrieval, graph, review, roles, and
  affected-test selection remain model-free. Optional failures are explicit
  readiness/fallback states.

## Consequences

Cartograph remains a single native artifact while users can choose local or
cloud inference. Managed llama.cpp convenience does not imply that Cartograph
owns the backend executable or model lifecycle. An endpoint/model/provider
change is visible in provenance and cannot cross-contaminate model-scoped
vectors or generated summaries.

Revisit this decision only if another backend can meet the same cross-platform
single-artifact, bounded-process, credential, model-identity, and live quality
gates without weakening external-provider support.
