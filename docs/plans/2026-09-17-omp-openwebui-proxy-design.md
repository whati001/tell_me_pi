# omp ↔ OpenWebUI Proxy — Design

Date: 2026-09-17
Status: implemented (updated after implementation; see "Deviations from the original design")

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
            omp --mode rpc --cwd /data/sessions/<chat>/work --session-dir /data/sessions/<chat>/omp
                --tools read,grep,glob,todo --approval-mode yolo
                --model <profile.model> --thinking <profile.thinking> [--resume <file>]
                                   │ host_tool_call "repo"
                                   ▼
            omp-proxy RepoTool ── git (execute-only; token passed via env to the git child only) ──▶ https://github.com/...
                                   └─ /data/mirrors/<repo>.git (shared, blobless mirror)
```

### Modules

| Module    | Responsibility |
|-----------|----------------|
| `config`  | Load `proxy.toml` (path from `OMP_PROXY_CONFIG`); validation (tool allowlist, reserved `extra_args` flags, repo names and URLs). |
| `rpc`     | Protocol types; frame decoder (v1 plus v2 `rpc_chunk` reassembly); `OmpProcess` (spawn, stdin writer, stdout reader task, responses matched by `id`, broadcast of session events, host-tool dispatch). |
| `session` | `SessionManager`: look up or start a session per chat key, lock per session, idle eviction, `max_live` LRU, runtime tool check, persisted `chat_id → session file` index, retention cleanup. |
| `repo`    | The `repo` host tool: `list`, `refs`, `checkout`. Mirror management with an in-process lock per repo; worktrees; the startup check that git processes are non-dumpable. |
| `api`, `translate`, `history` | `GET /v1/models`, `POST /v1/chat/completions` (SSE and non-streaming), bearer auth, `/healthz`, admin `DELETE /v1/sessions/{chat_id}`; omp-event → chunk translator; history-change detection. |
| `main`    | Secrets, privilege drop, git token self-check, maintenance timer, graceful shutdown (stops all omp children). |

### Session model: one warm omp process per chat, resumed on demand

- Chat key: the `X-OpenWebUI-Chat-Id` header (requires `ENABLE_FORWARD_USER_INFO_HEADERS=true` in
  OpenWebUI). Fallback when the header is missing: a hash of profile plus the first user message.
- A live process is reused. After `idle_timeout` it is stopped (stdin is closed and its stdout is
  drained). The next request resumes the session with `--resume <session-file>`, so follow-ups
  survive restarts.
- `max_live` caps concurrent processes; when full, the session idle the longest is stopped.
  `idle_timeout = 0` means one process per turn.
- One turn per session at a time. A new request for a busy chat cancels the running turn: the
  superseded response stream is ended (each turn has its own cancellation token), then the proxy
  sends `abort`, waits until `get_state` reports that omp is idle, and sends the new `prompt`.
  `abort_and_prompt` is not used, so events from the aborted run cannot leak into the new stream.
- On startup, the proxy negotiates protocol v2 when the ready frame advertises it, then sends
  `set_host_tools` with the `repo` tool (`loadMode: "essential"`). Host tools are activated
  automatically even with a restricted `--tools` list (see omp `session-tools.ts`).
- It then checks `get_state.dumpTools`: if omp enabled any tool besides `read`, `grep`, `glob`,
  `todo` and `repo` (e.g. through an omp setting), the process is stopped and the request fails.
- Control commands (everything except `prompt`) time out after 15 s. A hung omp is stopped, and the
  chat resumes from its session file on the next request.
- After each turn (`end_turn`) the proxy reconciles its state with omp: it reads
  `get_branch_messages`, and if the last user message omp stored is not the one just sent (a
  `prompt` can be acknowledged and still fail before omp stores it, e.g. without a login), that
  turn is dropped from the forwarded hashes, so the next request sends it again. It also refreshes
  the session file path from `get_state`.
- The `chat_id → session` index (`/data/index.json`) is written to a temporary file, fsynced and
  renamed. An index that cannot be parsed is moved to `index.json.corrupt` and the proxy starts
  with an empty index.

## The `repo` host tool (read-only git)

Implemented in Rust, inside the proxy. omp calls it through `host_tool_call`.

| Action | Arguments | Effect |
|---|---|---|
| `list` | — | Configured repos (name + description). |
| `refs` | `repo` | Tags (newest first) and branches, read from the mirror after a fetch. |
| `checkout` | `repo`, `ref` | Fetch the mirror (in-process `tokio::Mutex` per repo; only the proxy runs git, so no `flock`), resolve the ref to a commit, then `git worktree add --detach work/<repo>@<ref> <commit>` and write a marker file `work/.<repo>@<ref>.done` containing the commit; returns the path. Already checked out (directory and marker present) → returns the existing path. A directory without a marker is a leftover of an interrupted checkout and is recreated. |

- In the directory name, `_` in the ref is encoded as `_5f` and `/` as `_2f` (reversible), e.g.
  `release/1.4` → `backend@release_2f1.4`.
- **No push, commit, or write action exists.** `repo` must match a configured name; `ref` must match
  `^[A-Za-z0-9._/-]{1,200}$`, must not start with `-`, and must not contain `..`.
- The proxy runs git with an argument array (no shell).
- The token is passed only to that git child, through `GIT_CONFIG_COUNT` /
  `GIT_CONFIG_KEY_n=http.extraHeader` / `GIT_CONFIG_VALUE_n`. It never appears in argv and is never
  given to omp. git binaries are execute-only, so git processes are non-dumpable (see Security).
- Repository URLs must be `https://`, `http://` or `file://` without embedded credentials.
- Mirrors are created with `git clone --mirror --filter=blob:none` and shared by all chats.
- Progress (e.g. "fetching mirror…") is streamed with `host_tool_update`.
- On `host_tool_cancel`, the git child is killed.
- Nothing is checked out by default. The per-chat working directory starts empty.

### Skill `repo-checkout`

Lives in the project's `skills/repo-checkout/SKILL.md`, which is part of the skills mount
(`/data/home/.omp/agent/skills`). It tells
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
| Request for a chat that is still running | Cancel the running stream, `abort`, wait until idle, then handle as above |

The mapping from user turn to omp entry id comes from `get_branch_messages`
(`{messages: [{entryId, text}]}`); `branch(entryId)` re-roots the session before that user message.

OpenWebUI must not put retrieved context (files, web search) into the user message, or the hashes
change on every request: the compose file sets `RAG_SYSTEM_CONTEXT=true`, which puts it into the
system prompt instead.

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
- A newer request for the same chat → the superseded stream ends.
- `turn_timeout` → `abort` + error chunk.

### OpenWebUI background tasks

Titles, tags, and follow-up suggestions are **not** sent to the proxy. OpenWebUI's
`TASK_MODEL_EXTERNAL` points at a regular external model. The proxy does not special-case
`### Task:` prompts.

## LLM provider: OpenAI Codex (Pro subscription)

- Profiles use the `openai-codex` provider (e.g. `openai-codex/gpt-5.5`).
- Authentication: a one-time `/login` (OpenAI Codex) inside the container:
  `docker compose run --rm -it --user omp --entrypoint omp omp-proxy`.
- omp stores and refreshes the OAuth credential in `agent.db` in its agent directory. The image sets
  `HOME=/data/home` (on the writable volume), so the agent directory is `/data/home/.omp/agent`;
  `PI_CODING_AGENT_DIR` is not needed.
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
data_dir = "/data"            # sessions/<chat>/{work,omp}, mirrors/, index.json
idle_timeout = "15m"
max_live = 8
turn_timeout = "10m"
retention = "30d"

[omp]
binary = "/usr/local/bin/omp"
tools = ["read", "grep", "glob", "todo"]   # the only allowed tools; the repo host tool is always added
extra_args = []                            # flags the proxy controls are rejected
env_passthrough = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENAI_CODEX_OAUTH_TOKEN"]
reasoning = "field"

[[profile]]                    # each profile is one model in OpenWebUI
name = "omp-codex"
model = "openai-codex/gpt-5.5"
thinking = "medium"

[git]
token_file = "/run/secrets/git_token"
token_username = "x-access-token"
insecure_allow_exposed_token = false   # local development only

[[git.repo]]
name = "backend"
url = "https://github.com/acme/backend.git"
description = "Backend service"
```

## Security

- **Read-only tool set:** the config accepts only `read`, `grep`, `glob` and `todo` (aliases are
  normalized), so no `bash`, `eval`, `edit`, `write`, `ast_edit`, `task` or `debug`.
  `omp.extra_args` must not contain flags the proxy controls (`--tools`, `--mode`, `--cwd`,
  `--resume`, `--approval-mode`, extensions, …). After every omp start the proxy verifies the
  active tool list (`get_state.dumpTools`) and refuses anything besides these tools and `repo`.
  `lsp` is left out (no language servers in the image) and `ast_grep` is rejected by omp v18.2.3.
  `--approval-mode yolo` so no prompt can stall a turn.
- **Git:** the only network git access is the `repo` tool (fetch only). Use a read-only
  (fine-grained, Contents: read) token as well.
- **Token handling:** secrets are read from root-only files. The proxy then drops to uid 10001 with
  `setgid`/`setuid` and marks itself non-dumpable, so omp (same uid) cannot read its memory or
  `/proc/<pid>/environ`. omp is started with a cleared environment plus `env_passthrough`.
  That alone does not protect the token: git children receive it in their environment and run as
  the same uid. The image therefore makes `/usr/bin/git` and the ELF helpers in `/usr/lib/git-core`
  execute-only (`0711`); the kernel marks processes of unreadable binaries non-dumpable. At startup,
  with a token configured, the proxy starts a git process and checks that its
  `/proc/<pid>/environ` is unreadable; otherwise it refuses to start (unless
  `git.insecure_allow_exposed_token = true`, meant for local development).
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
     - installs `git`, `ca-certificates`, `curl`, `tini`, and makes the git binaries execute-only;
     - downloads the omp release binary (`omp-linux-{x64,arm64}`, pinned `ARG OMP_VERSION`) to
       `/usr/local/bin/omp`;
     - adds user `omp` (10001) with `HOME=/data/home`;
     - entrypoint `tini -- omp-proxy` (starts as root, drops to `omp`).
- **docker-compose.yml:**
  - `omp-proxy`:
    - volumes: `omp-data:/data`, `./proxy.toml:/etc/omp-proxy/proxy.toml:ro`,
      `./skills:/data/home/.omp/agent/skills:ro` (the `repo-checkout` skill plus your own skills);
    - secrets: `git_token`, `proxy_api_key`;
    - hardening as above; health check on `/healthz`.
  - `open-webui`:
    - `OPENAI_API_BASE_URLS="http://omp-proxy:8080/v1;<external>"` with matching keys;
    - `ENABLE_FORWARD_USER_INFO_HEADERS=true`, `TASK_MODEL_EXTERNAL=<external model>`,
      `RAG_SYSTEM_CONTEXT=true`;
    - volume `open-webui-data`, port 3000 (TLS reverse proxy in front).

## Testing

- **Fake omp:** a test binary that replays scripted JSONL. Integration tests cover streaming,
  non-streaming, regenerate/edit, abort on disconnect, a superseded busy turn, a crash followed by resume,
  idle eviction, and the `repo` host-tool round trip.
- **Unit tests:** frame decoder (v1, v2 chunks, malformed input), event translator, history-change
  detection, `repo` argument validation, config parsing.
- **`repo` integration test:** against a local bare git repo served over `file://`.
- **Opt-in end-to-end test:** a real `omp`, enabled with `OMP_E2E=1` (not implemented yet; the
  omp behaviour the proxy relies on was verified manually against the v18.2.3 release binary).

## Deviations from the original design

Changes made during implementation (the sections above already reflect them):

- Default tools are `read, grep, glob, todo`: no `lsp` (no language servers in the image), no
  `ast_grep` (rejected by omp v18.2.3). The config only accepts these tools, and the active tool
  list is verified at runtime.
- A busy chat is handled with per-turn cancellation + `abort` + wait-until-idle + `prompt` instead
  of `abort_and_prompt`.
- `HOME=/data/home` on the volume; omp's agent directory is `/data/home/.omp/agent` and skills are
  mounted at `/data/home/.omp/agent/skills`. No `PI_CODING_AGENT_DIR`.
- Repo locking is an in-process `tokio::Mutex` per repo instead of `flock`. Checkouts use encoded
  directory names and completion marker files.
- `end_turn` drops a turn that omp acknowledged but did not store, so it is sent again next time.
- git binaries are execute-only and the proxy checks at startup that git processes are
  non-dumpable; the non-dumpable proxy alone does not protect the token.
- OpenWebUI runs with `RAG_SYSTEM_CONTEXT=true` so history hashes stay stable.
- Control commands time out after 15 s; the index is fsynced and a corrupt index is set aside.
- `extra_args` defaults to `[]` and rejects proxy-controlled flags; repo URLs are restricted to
  https/http/file without credentials.
