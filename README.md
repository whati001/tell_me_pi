# tell_me_where — omp agent behind OpenWebUI

`omp-proxy` is an OpenAI-compatible server (`/v1/models`, `/v1/chat/completions`) that runs the
[oh-my-pi](https://github.com/can1357/oh-my-pi) coding agent (`omp --mode rpc`) for each
[OpenWebUI](https://github.com/open-webui/open-webui) chat. The agent is read-only. It can check out
releases of the configured repositories with its `repo` tool and answer questions about them.

Design: [`docs/plans/2026-09-17-omp-openwebui-proxy-design.md`](docs/plans/2026-09-17-omp-openwebui-proxy-design.md)

## How it works

- Each OpenWebUI chat (`X-OpenWebUI-Chat-Id`) gets its own omp process and an empty working
  directory.
- Idle processes are stopped and resumed from their session file on the next message. A new
  message for a chat that is still answering cancels the running answer first.
- The `repo` tool (`list`, `refs`, `checkout`) runs inside the proxy:
  - it keeps shared, blobless mirrors in `/data/mirrors`;
  - it checks refs out as `./<repo>@<ref>/` worktrees (`_` and `/` in the ref are written as
    `_5f` and `_2f`, e.g. `release/1.4` → `backend@release_2f1.4`);
  - the git token never reaches the agent;
  - there is no push.
- The agent's only other tools are `read`, `grep`, `glob` and `todo`, so it cannot run
  commands or write files.
- Skills come from `./skills` (mounted read-only). `skills/repo-checkout` teaches the agent how to
  pick refs.

## Deploy

1. Configure:

   ```sh
   cp proxy.example.toml proxy.toml     # profiles, repositories
   cp .env.example .env                 # OpenWebUI + task model settings
   mkdir -p secrets
   printf '%s' "<read-only git token>" > secrets/git_token
   printf '%s' "<random key>"          > secrets/proxy_api_key   # same value as PROXY_API_KEY in .env
   sudo chown 0:0 secrets/* && sudo chmod 600 secrets/*          # only root in the container can read them
   ```

   `chown 0:0` assumes rootful Docker without userns-remap; otherwise chown the files to the host
   uid that the container's root is mapped to.

   Use a read-only token, e.g. a GitHub fine-grained PAT with only *Contents: read* on the
   configured repositories.

2. Build, then log in to the OpenAI Codex subscription once. The login is stored in the `omp-data`
   volume and refreshed by omp:

   ```sh
   docker compose build
   docker compose run --rm -it --user omp --entrypoint omp omp-proxy
   # inside omp: /login  → choose OpenAI Codex (ChatGPT) → follow the browser flow → /exit
   ```

   `-it` is required: omp is interactive here. After you authorize in the browser, the redirect to
   `localhost:1455` fails to load (nothing listens there on your machine). Copy that full URL from
   the browser's address bar and paste it into omp when it asks for it.

3. Start:

   ```sh
   docker compose up -d
   ```

   OpenWebUI listens on `http://localhost:3000`. Put a TLS reverse proxy in front of it for cloud
   use. The proxy port is not published.

4. In OpenWebUI, pick a model named after a profile (e.g. `omp-codex`) and ask away.

The proxy does not handle OpenWebUI's background tasks (titles, tags, follow-ups): `TASK_MODEL`
must be a model served by the second connection (`TASK_API_BASE_URL`). The compose file sets
`RAG_SYSTEM_CONTEXT=true` so that attached files and web results go into the system prompt; the
proxy then still recognises follow-up messages as continuing the same history.

## Operations

- Logs: `docker compose logs -f omp-proxy`
- Delete a chat's agent state: `curl -X DELETE -H "Authorization: Bearer $PROXY_API_KEY" http://omp-proxy:8080/v1/sessions/<chat-id>`
  (from inside the compose network).
- Chats unused for `sessions.retention` are removed automatically.
- Update omp: change `OMP_VERSION` in the `Dockerfile` and run `docker compose build`.
- omp's settings live in the volume under `/data/home/.omp/agent`.
- An omp that does not answer a control command within 15 seconds is restarted; the chat resumes
  from its session file.
- The chat index (`/data/index.json`) is written atomically. If it is ever unreadable, the proxy
  moves it to `index.json.corrupt` and starts with an empty index (existing chats then start fresh
  agent sessions).

### Troubleshooting

- **The proxy refuses to start: "git child processes are readable by the agent".** A git token is
  configured, but git is not installed execute-only, so the agent could read the token from a
  running git process. The image takes care of this; for a local build see the `chmod 0711` step in
  the `Dockerfile`. `git.insecure_allow_exposed_token = true` skips the check (local development
  only).
- **A chat fails with "omp enabled tools outside the read-only set".** An omp setting added tools
  on top of `--tools` (for example `tools.xdev`, which may add an `xd://`-only `write` tool).
  Disable that setting in omp's config under `/data/home/.omp/agent`. With omp's default settings
  the active tools are `read`, `grep`, `glob`, `todo` and `repo`.
- **The proxy refuses to start with a config error about tools or `extra_args`.** Only `read`,
  `grep`, `glob` and `todo` are accepted, and `extra_args` must not contain flags the proxy sets
  itself (`--tools`, `--mode`, `--cwd`, `--resume`, `--approval-mode`, …).

## Security notes

- Read-only enforcement:
  - the config only accepts the tools `read`, `grep`, `glob` and `todo`, and rejects
    proxy-controlled flags (`--tools`, `--approval-mode`, `--resume`, …) in `omp.extra_args`;
  - after every omp start the proxy checks omp's active tool list and stops the process if it
    contains anything besides these tools and `repo`;
  - approval prompts are disabled (`--approval-mode yolo`) so no turn can stall.
- The proxy reads secrets as root, then switches to the `omp` user and marks itself non-dumpable.
- git runs from execute-only binaries, so the kernel marks git processes (which receive the token
  in their environment) non-dumpable and the agent cannot read their `/proc/<pid>/environ`. The
  proxy verifies this at startup and refuses to use the token otherwise.
- Repository URLs must be `https://`, `http://` or `file://` without embedded credentials;
  `http://` is rejected when a git token is configured.
- The container runs with:
  - a read-only root filesystem;
  - no capabilities except `SETUID`/`SETGID`;
  - `no-new-privileges` and resource limits.
- Known limitation: omp's `read` tool can read anything the `omp` user can read, including other
  chats' working directories and `/data/home/.omp/agent/agent.db` (the Codex login). This is fine
  for a single trusted user; revisit before sharing the instance.
- A ChatGPT Pro subscription is personal. Serving it to several users may violate OpenAI's terms and
  will hit per-account rate limits.

## Development

```sh
cargo test            # unit tests + integration tests against the fake omp (src/bin/fake_omp.rs)
cargo clippy --all-targets
```

`OMP_PROXY_CONFIG=./proxy.toml cargo run --bin omp-proxy` runs the proxy locally. Set
`sessions.data_dir` to a writable path, `omp.binary` to your local `omp`, and point (or remove)
`server.api_key_file` and `git.token_file` first. With a git token configured, a local git is
usually not execute-only: set `git.insecure_allow_exposed_token = true`, or leave `git.token_file`
unset for public repositories.
