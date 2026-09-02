# Contributing to Serica

Thanks for your interest in contributing! Serica is a single-binary DuckDuckGo-powered search engine written in Rust.

## Getting Started

```bash
git clone https://github.com/walkthestars/serica.git
cd serica
cargo build
cargo test --all-features
```

`redis-cache` is an optional, non-default feature, so a plain `cargo test` never compiles the Redis cache layer at all. CI runs `check`, `test`, and `clippy` with `--all-features` for exactly that reason — match it locally and you won't be surprised by a red CI on code you never built.

Minimum Rust version: **1.88+** (stable toolchain), enforced by `rust-version` in `Cargo.toml` and CI's `msrv` job. `time`/`cookie_store` (pulled in transitively via `reqwest`'s `cookies` feature) require 1.88.0 — this is the actual floor.

## Development Workflow

1. **Fork** the repository
2. **Create a branch** — use a descriptive name: `feat/add-redis-cache`, `fix/ddg-selector-update`
3. **Make your changes**
4. **Run the full test suite**: `cargo test --lib --all-features && cargo test --test api_test --all-features`
5. **Run clippy**: `cargo clippy --all-features --all-targets -- -D warnings`
6. **Sign off your commits** — see [Sign your work](#sign-your-work-dco) below
7. **Accept the CLA** (first-time contributors only, once ever) — see [Contributor License Agreement](#contributor-license-agreement-cla) below
8. **Run fmt**: `cargo fmt --all -- --check`
9. **Open a pull request** against `master`

## Sign your work (DCO)

Serica uses the [Developer Certificate of Origin](DCO) **and** a Contributor
License Agreement. They do different jobs and neither replaces the other: the
DCO certifies **where the code came from** (every commit, `Signed-off-by`),
the CLA grants the project's maintainer the rights described in
[CLA.md](CLA.md) (once per contributor). You keep the copyright in your
contribution under both.

All you do is add a `Signed-off-by` line to each commit, certifying that you
wrote the code (or otherwise have the right to submit it) and are contributing
it under AGPL-3.0-only. Git writes the line for you with `-s`:

```bash
git commit -s -m "feat(api): add rate limiting middleware"
```

which appends:

```
Signed-off-by: Your Name <your.email@example.com>
```

The name and email must match the commit author. Use your real name; anonymous
and pseudonymous sign-offs are fine as long as it is the identity you actually
go by. CI enforces this on every commit in a pull request.

**Forgot to sign off?** Amend the last commit:

```bash
git commit --amend -s --no-edit
```

Or sign off every commit on your branch at once:

```bash
git rebase --signoff master
```

Then force-push your branch.

Git has no config option that signs off `git commit` automatically (`format.signOff`
only affects `format-patch`), so if you'd rather not think about it, either alias it:

```bash
git config --global alias.ci "commit -s"
```

or drop a `prepare-commit-msg` hook in this clone:

```bash
printf '#!/bin/sh\ngit interpret-trailers --if-exists doNothing --trailer \\\n  "Signed-off-by: $(git config user.name) <$(git config user.email)>" \\\n  --in-place "$1"\n' > .git/hooks/prepare-commit-msg && chmod +x .git/hooks/prepare-commit-msg
```

## Contributor License Agreement (CLA)

In addition to the DCO, Serica asks first-time contributors to accept the
[Contributor License Agreement](CLA.md) — **once, ever**, not per pull
request. It grants the project's maintainer the right to license your
contribution as part of the project under other terms (for example, a
commercial license), while your contribution itself remains public under
AGPL-3.0-only as part of the project. You keep the copyright. The full text
is in [CLA.md](CLA.md); it is short and worth reading.

**How to sign:** on your first pull request, the CLA Assistant bot will
comment with instructions. Read [CLA.md](CLA.md), then comment this exact
phrase on the pull request:

```
I have read the CLA Document and I hereby sign the CLA
```

That's it — the bot marks the check green and records your signature. The
signature is stored in the `cla-signatures` branch as a JSON record with your
GitHub identity and the date, so the project's licensing history is fully
auditable in the open. Subsequent pull requests never ask again. Repository
bots (`dependabot[bot]` and friends) are exempt.

## Code Style

- Follow `rustfmt` defaults (enforced in CI)
- Use `clippy` with `-D warnings` (enforced in CI)
- Prefer `expect` over `unwrap` for recoverable failures
- Use `tracing` for logging (not `println!` or `eprintln!`)
- Document public APIs with doc comments (`///`)

## Testing

- **Unit tests** go in `#[cfg(test)]` modules at the bottom of each source file
- **Integration tests** go in `tests/api_test.rs`
- **HTML fixtures** go in `tests/fixtures/` — real captured pages, not synthetic lookalikes
- Every new feature should include tests for the happy path + error path + edge cases
- The suite runs offline: search is mocked with `wiremock` via `DuckDuckGoAdapter::with_base_url`. The few extract tests that genuinely need live network (the SSRF guard rejects the loopback address a local mock would have to bind) are marked `#[ignore]` and are not run by CI — run them deliberately with `cargo test -- --ignored`.

## Commit Messages

Follow the [Conventional Commits](https://www.conventionalcommits.org/) format:

```
feat(api): add rate limiting middleware
fix(cors): invert wildcard/permissive logic
docs(readme): document environment variables
chore(deps): update rustls-pki-types to 1.15.1
```

## Pull Request Checklist

- [ ] All tests pass (`cargo test --lib --all-features && cargo test --test api_test --all-features`)
- [ ] No clippy warnings (`cargo clippy --all-features --all-targets -- -D warnings`)
- [ ] Code is formatted (`cargo fmt --all -- --check`)
- [ ] New features include tests
- [ ] Documentation is updated if needed
- [ ] CHANGELOG.md is updated (under [Unreleased])

## Project Structure

```
src/
├── main.rs                     # Entry point, CLI parsing, graceful shutdown
├── lib.rs                      # Library root, module re-exports
├── config.rs                   # Layered configuration (figment) + startup validation
├── error.rs                    # Error types with IntoResponse
├── gateway/
│   ├── mod.rs                  # Shared gateway logic: safe-search filter, cache keys
│   ├── http.rs                 # HTTP routes, middleware, state
│   └── mcp.rs                  # MCP stdio JSON-RPC server
├── engines/
│   ├── mod.rs                  # Engine module
│   └── duckduckgo.rs           # DDG adapter, health probe, 202-challenge detection
├── models/
│   ├── mod.rs
│   ├── search.rs               # Search request/response types
│   └── extract.rs              # Extract request/response types, Options builder
├── cache/
│   ├── mod.rs                  # CacheLayer trait (byte-blob, response-type-agnostic)
│   ├── noop.rs                 # Fallback no-op cache
│   └── redis.rs                # Redis-backed cache (feature `redis-cache`)
└── util/
    ├── mod.rs
    ├── extract.rs              # Shared extract pipeline (run_extract) + concurrency limiter
    ├── fetch.rs                # Capped body reads, content-type gating
    ├── ssrf.rs                 # URL validation, IP pinning, per-hop redirect re-validation
    ├── prune.rs                # lol_html script/style pre-prune + whole-doc text fallback
    ├── codespan_fixup.rs       # Local patch: rs-trafilatura whitespace-span pruning
    ├── codeblock_linebreaks.rs # Local patch: quick_html2md <pre> sibling joining
    ├── metrics.rs              # Atomic counter metrics
    └── tracing.rs              # Logging init (stdout; stderr under `serica mcp`)
```

The two `util/` files marked *local patch* work around upstream bugs in pinned
dependency versions. Each carries a regression test that pins the **current,
broken** upstream behavior, so a dependency bump that fixes the bug upstream
fails loudly rather than silently double-fixing. If you bump `rs-trafilatura`
or `lol_html`, expect those to fire — that is the design, not a breakage.

## Architecture

The two gateways are siblings, not layers — each is an entry point onto the
same shared core.

```
              ┌─ HTTP Gateway (axum) ─┐      ┌─ search  → DuckDuckGo adapter
   Client ────┤                       ├──────┤            (reqwest + scraper)
              └─ MCP Gateway (stdio) ─┘      └─ extract → util::extract::run_extract
                                                          (SSRF-validated fetch → prune
                                                           → rs-trafilatura → fallbacks)

   HTTP middleware, outermost first:
     Tracing + Request ID → CORS (only if configured) → Compression → Timeout
       → Rate limit (tower_governor token bucket, keyed on peer IP;
          /api/v1/extract gets its own bucket at ¼ the general budget)

   Shared by both gateways:
     CacheLayer (Redis or Noop) · MetricsCollector · ExtractLimiter
```

Rate limiting and the extract concurrency limit are two different mechanisms
and are easy to confuse. The **rate limiter** is a per-peer-IP token bucket
that *rejects* with `429`. The **`ExtractLimiter`** is a process-wide semaphore
(`server.max_concurrent_extracts`) that makes extract requests *wait* — up to
`extract.timeout_ms`, after which they fail with `504`.

## Questions?

Open an issue or start a discussion on GitHub.
