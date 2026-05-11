#!/usr/bin/env bash
# agent-opencode.sh — invoke opencode against the LiteLLM proxy or sub2api.
#
# Reads $AR_TASK_FILE, $AR_WORKTREE, $AR_BRANCH, $AR_SCORE, $AR_PROGRAM
# from the orchestrator. Constructs a single prompt that is program.md +
# the task file, and runs opencode in headless mode.
#
# Provider routing:
#   claude-* models  → sub2api (OAuth subscription, zero marginal cost)
#   everything else  → LiteLLM proxy (local GPU / OpenRouter)
#
# Env vars expected from .env or shell:
#   LITELLM_BASE_URL    default http://localhost:4000
#   LITELLM_API_KEY     LiteLLM master key
#   SUB2API_BASE_URL    default http://sub2api.apps.svc.cluster.local:8080/v1
#   SUB2API_API_KEY     sub2api key
#   AR_MODEL            default traitor/qwen3-8b
set -euo pipefail

: "${AR_TASK_FILE:?required}"
: "${AR_WORKTREE:?required}"
: "${AR_PROGRAM:?required}"

LITELLM_BASE_URL="${LITELLM_BASE_URL:-http://localhost:4000}"
LITELLM_API_KEY="${LITELLM_API_KEY:-}"
SUB2API_BASE_URL="${SUB2API_BASE_URL:-http://sub2api.apps.svc.cluster.local:8080/v1}"
SUB2API_API_KEY="${SUB2API_API_KEY:-}"
AR_MODEL="${AR_MODEL:-traitor/qwen3-8b}"

# Prefer the newer downloaded binary over nixpkgs's older system one — system
# opencode 1.1.14 doesn't have --dir / --dangerously-skip-permissions.
# Override with OPENCODE_BIN if you need a different binary.
OPENCODE_BIN="${OPENCODE_BIN:-$HOME/.local/bin/opencode}"

# Route provider: claude-* → sub2api, everything else → litellm
if [[ "$AR_MODEL" == claude-* ]]; then
    if [[ -z "$SUB2API_API_KEY" ]]; then
        echo "agent-opencode: claude model requested but SUB2API_API_KEY not set" >&2
        exit 7
    fi
    PROVIDER_NAME="sub2api"
    PROVIDER_BASE_URL="$SUB2API_BASE_URL"
    PROVIDER_API_KEY="$SUB2API_API_KEY"
else
    if [[ -z "$LITELLM_API_KEY" ]]; then
        echo "agent-opencode: LITELLM_API_KEY not set" >&2
        exit 7
    fi
    PROVIDER_NAME="litellm"
    PROVIDER_BASE_URL="$LITELLM_BASE_URL"
    PROVIDER_API_KEY="$LITELLM_API_KEY"
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

export OPENAI_BASE_URL="$PROVIDER_BASE_URL"
export OPENAI_API_KEY="$PROVIDER_API_KEY"

mkdir -p "$OC_CFG/opencode"
cat > "$OC_CFG/opencode/config.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "provider": {
    "sub2api": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "sub2api (Anthropic OAuth)",
      "options": {
        "baseURL": "$SUB2API_BASE_URL",
        "apiKey": "$SUB2API_API_KEY"
      },
      "models": {
        "claude-sonnet-4-6": {},
        "claude-haiku-4-5-20251001": {},
        "claude-opus-4-6": {}
      }
    },
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
  "small_model": "$PROVIDER_NAME/$AR_MODEL",
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
    --model "$PROVIDER_NAME/$AR_MODEL" \
    --dir "$AR_WORKTREE" \
    --dangerously-skip-permissions \
    "$(cat "$PROMPT")"
