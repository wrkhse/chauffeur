# Contributing

## One-time local setup

Enable this repository's pre-commit hook once per checkout:

    git config core.hooksPath .githooks

On each commit the hook formats only staged files: rustfmt for .rs paths (edition 2024) and tidy v2.0.0 for the other staged files, using tidy's defaults (this repository has no tidy config). It re-stages the formatted result. tidy skips file types it does not recognize; a recognized file it cannot parse, a partially staged file, or a tidy download/checksum/platform failure stops the commit with a named reason. It does not run clippy, compile or tests. The tidy binary's SHA-256 is verified on first use and it is cached at ~/.cache/wrkhse/tidy-<sha>/; the URL and pin live in .github/tidy.env.
