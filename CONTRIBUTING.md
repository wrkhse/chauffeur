# Contributing

## One-time local setup

Enable this repository's pre-commit hook once per checkout:

    git config core.hooksPath .githooks

On each commit the hook formats only staged files: rustfmt for .rs paths (edition 2024) and tidy v2.0.0 for the other staged files, using the repository's tidy config. It re-stages the formatted result and refuses a commit only when a file cannot be formatted. It does not run clippy, compile or tests. A partially staged file is refused with a named reason. The tidy binary's SHA-256 is verified on first use and it is cached at ~/.cache/wrkhse/tidy-<sha>/; the URL and pin live in .github/tidy.env.
