#!/usr/bin/env bash
# agent-opencode.sh — invoke opencode against the LiteLLM proxy.
#
# Reads $AR_TASK_FILE, $AR_WORKTREE, $AR_BRANCH, $AR_SCORE, $AR_PROGRAM
# from the orchestrator. Constructs a single prompt (program.md + task file)
# and runs opencode in headless mode.
#
# All models route through LiteLLM ($LITELLM_BASE_URL). LiteLLM handles
# upstream routing: claude-* → Anthropic (via OpenRouter or sub2api proxy),
# free-tier → OpenRouter, local → vllm on traitor/seir.
#
# Confirmed working models (opencode tool-use validated 2026-05-11):
#   claude-haiku-4-5       — cheap, reliable tool-use
#   claude-sonnet-4-6      — mid-tier, for harder tasks
#   deepseek-chat-v3-1     — free, decent tool-use
#
# NOT working (no file writes despite claiming to):
#   traitor/qwen3-8b       — deregistered from LiteLLM
#   qwen3-coder-free       — generates intent but skips tool calls
#   qwen3-next-80b-free    — same
#
# Env vars from .env:
#   LITELLM_BASE_URL   default http://localhost:4000
#   LITELLM_API_KEY    LiteLLM master key
#   AR_MODEL           set by accountant (see current-recommendations.toml)
set -euo pipefail

: "${AR_TASK_FILE:?required}"
: "${AR_WORKTREE:?required}"
: "${AR_PROGRAM:?required}"

LITELLM_BASE_URL="${LITELLM_BASE_URL:-http://localhost:4000}"
LITELLM_API_KEY="${LITELLM_API_KEY:-}"
AR_MODEL="${AR_MODEL:-claude-haiku-4-5}"

# Prefer the newer npm binary (1.14+) which has --dir and --dangerously-skip-permissions.
# Note: use the npm binary for `run` mode — only ACP mode crashes on NixOS.
OPENCODE_BIN="${OPENCODE_BIN:-$HOME/.local/bin/opencode}"

if [[ -z "$LITELLM_API_KEY" ]]; then
    echo "agent-opencode: LITELLM_API_KEY not set" >&2
    exit 7
fi

if [[ ! -x "$OPENCODE_BIN" ]]; then
    echo "agent-opencode: $OPENCODE_BIN not executable" >&2
    exit 8
fi

cd "$AR_WORKTREE"

PROMPT=$(mktemp)
OC_CFG=$(mktemp -d)
trap 'rm -f "$PROMPT"; rm -rf "$OC_CFG"' EXIT
{
    cat "$AR_PROGRAM"
    printf '\n\n---\n\n## Your task\n\n'
    cat "$AR_TASK_FILE"
} > "$PROMPT"

export OPENAI_BASE_URL="$LITELLM_BASE_URL"
export OPENAI_API_KEY="$LITELLM_API_KEY"

mkdir -p "$OC_CFG/opencode"
cat > "$OC_CFG/opencode/config.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "provider": {
    "litellm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "LiteLLM",
      "options": {
        "baseURL": "$LITELLM_BASE_URL",
        "apiKey": "$LITELLM_API_KEY"
      },
      "models": {
        "$AR_MODEL": {}
      }
    }
  },
  "small_model": "litellm/$AR_MODEL",
  "compaction": {
    "auto": true,
    "prune": true,
    "tail_turns": 2,
    "preserve_recent_tokens": 6000,
    "reserved": 2000
  }
}
JSON
XDG_CONFIG_HOME="$OC_CFG" "$OPENCODE_BIN" run \
    --model "litellm/$AR_MODEL" \
    --dir "$AR_WORKTREE" \
    --dangerously-skip-permissions \
    "$(cat "$PROMPT")"
