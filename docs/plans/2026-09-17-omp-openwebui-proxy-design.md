# omp ↔ OpenWebUI Proxy — Design

Date: 2026-09-17
Status: validated, ready for implementation planning

## Goal

Let OpenWebUI act as the chat UI for the [oh-my-pi](https://github.com/can1357/oh-my-pi) (`omp`)
coding agent, including its skills, so users can ask questions about our codebases, and about
specific releases of them.

- OpenWebUI talks to a Rust proxy through the OpenAI Chat Completions API.
- The proxy drives `omp --mode rpc` (newline-delimited JSON over stdio). The Node SDK is not used.
- Multiple parallel conversations, with follow-up questions, are supported.
- The agent is **read-only**. It can fetch and check out repository refs (tags or branches) into a
  per-chat working directory. It cannot push, write files, or run shell commands.
- The proxy is deployed with a Dockerfile and docker-compose.

Out of scope for now: agent write/edit tools, repositories chosen per user, and OpenWebUI
background tasks (these go to an external LLM, see below).

## Architecture

```
open-webui ──HTTP (OpenAI API, Bearer key, X-OpenWebUI-Chat-Id)──▶ omp-proxy (Rust, axum)
                                                                     │
                                                              SessionManager
                                                     chat_id → SessionHandle (idle eviction,
                                                     max_live cap, persisted index)
                                                                     │ spawns, one per chat
                                                                     ▼
            omp --mode rpc --cwd /data/sessions/<chat>/work --tools read,grep,glob,ast_grep,lsp,todo
                         --model <profile.model> --thinking <profile.thinking> [--resume <file>]
                                   │ host_tool_call "repo"
                                   ▼
            omp-proxy RepoTool ── git (token passed via env to the git child only) ──▶ https://github.com/...
                                   └─ /data/mirrors/<repo>.git (shared, blobless mirror)
```

### Modules

| Module    | Responsibility |
|-----------|----------------|
| `config`  | Load `proxy.toml`; environment variables override it; validation. |
| `rpc`     | Protocol types; frame decoder (v1 plus v2 `rpc_chunk` reassembly); `OmpProcess` (spawn, stdin writer, stdout reader task, responses matched by `id`, broadcast of session events, host-tool dispatch). |
| `session` | `SessionManager`: look up or start a session per chat key, lock per session, idle eviction, `max_live` LRU, persisted `chat_id → session file` index, retention cleanup. |
| `repo`    | The `repo` host tool: `list`, `refs`, `checkout`. Mirror management with a lock per repo; worktrees. |
| `openai`  | `GET /v1/models`, `POST /v1/chat/completions` (SSE and non-streaming), omp-event → chunk translator, history-change detection. |
| `main`    | axum router, bearer auth, `/healthz`, admin `DELETE /v1/sessions/{chat_id}`, privilege drop, graceful shutdown (stops all omp children). |

### Session model: one warm omp process per chat, resumed on demand

- Chat key: the `X-OpenWebUI-Chat-Id` header (requires `ENABLE_FORWARD_USER_INFO_HEADERS=true` in
  OpenWebUI). Fallback when the header is missing: a hash of profile plus the first user message.
- A live process is reused. After `idle_timeout` it is stopped (stdin is closed and its stdout is
  drained). The next request resumes the session with `--resume <session-file>`, so follow-ups
  survive restarts.
- `max_live` caps concurrent processes; when full, the session idle the longest is stopped.
  `idle_timeout = 0` means one process per turn.
- One turn per session at a time. A new request for a busy chat → `abort_and_prompt`.
- On startup, the proxy negotiates protocol v2 when the ready frame advertises it, then sends
  `set_host_tools` with the `repo` tool (`loadMode: "essential"`). Host tools are activated
  automatically even with a restricted `--tools` list (see omp `session-tools.ts`).

## The `repo` host tool (read-only git)

Implemented in Rust, inside the proxy. omp calls it through `host_tool_call`.

| Action | Arguments | Effect |
|---|---|---|
| `list` | — | Configured repos (name + description). |
| `refs` | `repo` | Tags (newest first) and branches, read from the mirror after a fetch. |
| `checkout` | `repo`, `ref` | Fetch the mirror (`flock` per repo), then `git worktree add --detach work/<repo>@<ref> <ref>`; returns the path. Already checked out → returns the existing path. |

- **No push, commit, or write action exists.** `repo` must match a configured name; `ref` must match
  `^[A-Za-z0-9._/-]{1,200}$`, must not start with `-`, and must not contain `..`.
- The proxy runs git with an argument array (no shell).
- The token is passed only to that git child, through `GIT_CONFIG_COUNT` /
  `GIT_CONFIG_KEY_n=http.extraHeader` / `GIT_CONFIG_VALUE_n`. It never appears in argv and is never
  given to omp.
- Mirrors are created with `git clone --mirror --filter=blob:none` and shared by all chats.
- Progress (e.g. "fetching mirror…") is streamed with `host_tool_update`.
- On `host_tool_cancel`, the git child is killed.
- Nothing is checked out by default. The per-chat working directory starts empty.

### Skill `repo-checkout`

Lives in the project's `skills/repo-checkout/SKILL.md`, which is part of the skills mount. It tells
the agent:

- to check out code only when the question needs it;
- to map "release 1.4" to the newest matching `v1.4.*` tag using `refs`;
- to check out two refs side by side when comparing versions;
- to cite paths relative to the worktree.

## OpenAI API translation

### Request handling

OpenWebUI sends the full history on every request; omp already holds it. The proxy forwards only
the newest user message. Per chat it stores the hashes of the user messages it has already
forwarded:

| Incoming history | Action |
|---|---|
| Known history + one new user message | `prompt` |
| Identical to the last request (Regenerate) | `branch` back to before the last user message, then `prompt` |
| An earlier message changed (Edit) | `branch` back to the changed message. If the branch entry can't be found, start a new session whose prompt includes a short transcript of the previous conversation. |
| Request for a chat that is still running | `abort_and_prompt` |

The mapping from user turn to omp entry id comes from the session's message entries. Verify the
exact field during implementation (`get_messages` / `get_branch_messages`).

Base64 `image_url` parts are converted to omp `ImageContent`.

### Events → chunks

| omp | OpenAI chunk |
|---|---|
| `message_update` / `thinking_delta` | `delta.reasoning_content` (config: `reasoning = "field" \| "think_tags" \| "off"`) |
| `message_update` / `text_delta` | `delta.content` |
| `tool_execution_start` / `_end` | Collapsible `<details>` block in `content` (tool name, short argument summary, ✓/✗) |
| `agent_end` with `isTerminal !== false` | `finish_reason: "stop"`, usage chunk, `[DONE]` |
| `agent_end` with `isTerminal: false` | Ignored; the stream continues |
| Failure response, omp exit | Error chunk, then the stream closes; the session is marked dead and resumes from file on the next request |

- `stream: false` collects the same content into one response.
- The client disconnecting → `abort`.
- `turn_timeout` → `abort` + error chunk.

### OpenWebUI background tasks

Titles, tags, and follow-up suggestions are **not** sent to the proxy. OpenWebUI's
`TASK_MODEL_EXTERNAL` points at a regular external model. The proxy does not special-case
`### Task:` prompts.

## LLM provider: OpenAI Codex (Pro subscription)

- Profiles use the `openai-codex` provider (e.g. `openai-codex/gpt-5.5`).
- Authentication: a one-time `/login openai-codex` inside the container:
  `docker compose run --rm -it omp-proxy omp`.
- omp stores and refreshes the OAuth credential in `agent.db` in its agent directory. That directory
  is on the writable volume (`PI_CODING_AGENT_DIR=/data/omp-agent`).
- `~/.codex` is **not** mounted. omp doesn't use it for credentials, and skills come from the
  project's own skills mount (see Deployment).
- `env_passthrough` keeps `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and `OPENAI_CODEX_OAUTH_TOKEN` for
  reference. They are passed through only if they are set.
- Note: a Pro subscription belongs to one person. Sharing it with several OpenWebUI users may
  violate OpenAI's terms and will hit per-account rate limits.

## Configuration (`/etc/omp-proxy/proxy.toml`)

```toml
[server]
listen = "0.0.0.0:8080"
api_key_file = "/run/secrets/proxy_api_key"

[sessions]
data_dir = "/data"            # sessions/<chat>/work, omp-agent/, mirrors/, index
idle_timeout = "15m"
max_live = 8
turn_timeout = "10m"
retention = "30d"

[omp]
binary = "/usr/local/bin/omp"
tools = ["read", "grep", "glob", "ast_grep", "lsp", "todo"]   # the repo host tool is always added
extra_args = ["--no-title"]
env_passthrough = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENAI_CODEX_OAUTH_TOKEN"]
reasoning = "field"

[[profile]]                    # each profile is one model in OpenWebUI
name = "omp-codex"
model = "openai-codex/gpt-5.5"
thinking = "medium"

[git]
token_file = "/run/secrets/git_token"

[[git.repo]]
name = "backend"
url = "https://github.com/acme/backend.git"
description = "Backend service"
```

## Security

- **Read-only tool set:** no `bash`, `eval`, `edit`, `write`, `task`. `--approval-mode yolo` so no
  prompt can stall a turn.
- **Git:** the only network git access is the `repo` tool (fetch only). Use a read-only
  (fine-grained, Contents: read) token as well.
- **Token handling:** secrets are read from root-only files. The proxy then drops to uid 10001 with
  `setgid`/`setuid`, which makes its `/proc/<pid>/environ` unreadable to omp. omp is started with a
  cleared environment plus `env_passthrough`.
- **Container:** `read_only` root filesystem with a tmpfs for `/tmp`, `cap_drop: ALL` plus
  `SETUID`/`SETGID`, `no-new-privileges`, and pids, memory, and CPU limits. The proxy port is not
  published; only OpenWebUI reaches it.
- **Known residual risks:**
  - omp's `read` tool can read any file the omp user can read, including other chats' working
    directories and `agent.db` (the Codex refresh token). Acceptable for a single trusted user;
    revisit (per-session sandbox, auth broker) before opening it to more users.
  - Verify whether omp offers read-path restrictions.

## Deployment

- **Dockerfile (multi-stage):**
  1. `rust:1-bookworm` builds `omp-proxy`.
  2. `debian:bookworm-slim` runtime:
     - installs `git`, `ca-certificates`, `tini`;
     - installs omp from the release installer (`PI_INSTALL_DIR=/usr/local/bin`, pinned
       `ARG OMP_VERSION`);
     - adds user `omp` (10001);
     - entrypoint `tini -- omp-proxy`.
- **docker-compose.yml:**
  - `omp-proxy`:
    - volumes: `omp-data:/data`, `./proxy.toml:/etc/omp-proxy/proxy.toml:ro`,
      `./skills:/data/omp-agent/skills:ro` (the `repo-checkout` skill plus your own skills);
    - secrets: `git_token`, `proxy_api_key`;
    - hardening as above; health check on `/healthz`.
  - `open-webui`:
    - `OPENAI_API_BASE_URLS="http://omp-proxy:8080/v1;<external>"` with matching keys;
    - `ENABLE_FORWARD_USER_INFO_HEADERS=true`, `TASK_MODEL_EXTERNAL=<external model>`;
    - volume `open-webui-data`, port 3000 (TLS reverse proxy in front).

## Testing

- **Fake omp:** a test binary that replays scripted JSONL. Integration tests cover streaming,
  non-streaming, regenerate/edit, abort on disconnect, abort_and_prompt, a crash followed by resume,
  idle eviction, and the `repo` host-tool round trip.
- **Unit tests:** frame decoder (v1, v2 chunks, malformed input), event translator, history-change
  detection, `repo` argument validation, config parsing.
- **`repo` integration test:** against a local bare git repo served over `file://`.
- **Opt-in end-to-end test:** a real `omp`, enabled with `OMP_E2E=1`.
