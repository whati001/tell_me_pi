---
name: repo-checkout
description: Check out a repository at a specific release, tag or branch with the `repo` tool before answering questions about code. Use when a question concerns source code, a release, a version, or differences between versions.
---

# Checking out repositories

You start in an empty working directory. Code is only available after you check it out with the
`repo` tool. The tool is read-only: it can list, fetch and check out, never push or modify.

## When to check out

- Check out code only when the question is about source code, configuration, behaviour of a
  specific version, or a change between versions.
- Do not check out anything for general questions that do not need the code.
- Reuse checkouts that already exist in the working directory (`glob` for `*@*`).

## Picking the right ref

1. If the repository is unclear, call `repo` with `{"action": "list"}` and pick the matching one,
   or ask the user when several could match.
2. Call `{"action": "refs", "repo": "<name>"}`. Tags are listed newest version first.
3. Map the user's wording to a ref:
   - "release 1.4" / "version 1.4" → the newest tag matching `v1.4.*` (or `1.4.*`).
   - "v1.4.2" → that exact tag.
   - "latest release" → the first tag in the list.
   - "current", "main", "development" or no version given → the default branch (`main` or
     `master`).
4. If nothing matches, say so and list the closest refs instead of guessing.

## Checking out and reading

- `{"action": "checkout", "repo": "<name>", "ref": "<ref>"}` creates `./<name>@<ref>/`. In the
  directory name, `_` in the ref becomes `_5f` and `/` becomes `_2f` (e.g. `release/1.4` →
  `./backend@release_2f1.4/`). Use the directory name from the tool result.
- Then use `grep`, `glob` and `read` with paths under that directory.
- To compare versions, check out both refs and inspect the same paths in both directories.

## Answering

- State which repository and ref your answer is based on.
- Cite files as `path/to/file.rs:123` relative to the repository root (without the
  `<name>@<ref>/` prefix) and quote the relevant lines.
