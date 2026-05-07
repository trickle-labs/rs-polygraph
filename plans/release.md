# Release Plan — CI, Publishing, and Documentation

**Status**: complete
**Updated**: 2026-05-07

This plan covers the infrastructure work needed to ship `polygraph` as a
production-grade crate on crates.io: GitHub Actions CI, automated publishing,
docs.rs API documentation, and an optional GitHub Pages guide site.

---

## 1. GitHub Actions CI

Add `.github/workflows/ci.yml` triggered on every push and pull request.

### Jobs

| Job | Command | Purpose |
|-----|---------|---------|
| `test` | `cargo test` | Unit + integration tests |
| `tck` | `cargo test --test tck` | Full 3828-scenario TCK suite |
| `difftest` | `cargo test -p polygraph-difftest` | 204 curated differential queries |
| `clippy` | `cargo clippy -- -D warnings` | Lint gate |
| `fmt` | `cargo fmt --check` | Format gate |
| `doc` | `cargo doc --no-deps` | Docs build must be warning-free |

**Matrix**: test on `stable` and `beta`. TCK/difftest on `stable` only (slow).

**Rust version pinning**: lock `rust-toolchain.toml` to a specific stable
channel so `MSRV` is checked and CI is reproducible.

### MSRV

Declare `rust-version = "1.80"` (or current minimum) in `Cargo.toml`. Add a
CI job that runs on that version.

---

## 2. Automated Publishing

Add `.github/workflows/release.yml` triggered on `v*` tag push.

### Steps

1. Validate that `Cargo.toml` version matches the tag (`v0.7.0` ↔ `version = "0.7.0"`).
2. Run the full CI matrix.
3. `cargo publish --token ${{ secrets.CARGO_REGISTRY_TOKEN }}` for `polygraph`.
4. If `polygraph-difftest` is publishable: publish it too (or skip if it's dev-only).

**Secret**: `CARGO_REGISTRY_TOKEN` stored in GitHub repo secrets.

---

## 3. API Documentation (docs.rs)

docs.rs auto-publishes API docs when a crate is pushed to crates.io — no extra
work once the crate is published.  However, to make docs high quality:

- Add `#![doc = include_str!("../README.md")]` to `src/lib.rs` so the crate-level
  doc is the README.
- Ensure every public type, function, and trait has a doc comment.
- Add `[package.metadata.docs.rs]` to `Cargo.toml` to enable all features:

```toml
[package.metadata.docs.rs]
all-features = true
rustdoc-args = ["--cfg", "docsrs"]
```

---

## 4. GitHub Pages Guide Site (optional but recommended)

A transpiler library benefits from a small cookbook beyond the API reference:
query patterns, RDF encoding choices, `TargetEngine` integration guide, known
limitations.

**Tooling**: `mdBook` — the Rust ecosystem standard for guide sites.

### Structure

```
docs/
  book.toml
  src/
    SUMMARY.md
    introduction.md
    quickstart.md
    query-patterns.md       # MATCH, WITH, aggregation examples
    rdf-encoding.md         # RDF-star vs reification + how to read output triples
    target-engines.md       # Implementing TargetEngine for a new engine
    limitations.md          # Unsupported constructs + why
    changelog.md
```

### Deployment

Add `.github/workflows/docs.yml`:
- Trigger: push to `main`
- Steps: `mdbook build`, deploy `docs/book/` to `gh-pages` branch via
  `actions/deploy-pages` or `peaceiris/actions-gh-pages`

---

## 5. Crate Metadata Checklist

Before first `cargo publish`:

| Field | Required | Value |
|-------|----------|-------|
| `name` | ✓ | `polygraph` |
| `version` | ✓ | current semver |
| `description` | ✓ | present |
| `license` | ✓ | `Apache-2.0` |
| `repository` | ✓ | GitHub URL |
| `documentation` | — | auto-set by docs.rs; or `https://docs.rs/polygraph` |
| `homepage` | — | GitHub Pages URL once deployed |
| `keywords` | recommended | `["cypher", "sparql", "gql", "transpiler", "graph"]` |
| `categories` | recommended | `["parser-implementations", "database-interfaces"]` |
| `readme` | recommended | `"README.md"` |
| `exclude` | recommended | `["tests/tck/features/**", "plans/**", "benches/**"]` to keep crate download small |

---

## 6. Release Procedure

See [`.github/skills/release-polygraph/SKILL.md`](../.github/skills/release-polygraph/SKILL.md)
for the step-by-step checklist used when cutting a release.
