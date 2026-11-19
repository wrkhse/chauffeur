---
name: chauffeur-docs
description: MUST use when writing or editing any Markdown in the Chauffeur repository, including the slice pages in docs/plans/slices/, the plan, plugin, integration, CLI and migration pages, and module READMEs, and when a slice is complete and its end-to-end recording is due. Adds Chauffeur's names, page layout, docs site checks and slice recordings to the spec-docs skill.
---

# Chauffeur docs

Follow the `spec-docs` skill: its voice, code examples, line length, slice
pages and Structure apply here. This adds what is Chauffeur's own.

## The site

Every Markdown file in the repository is a page on the docs site, built by
VitePress in `site/` and published to
`https://chauffeur-docs.stevenjpx2.workers.dev`. `.agents/` and build
output are left out.

```sh
cd site
bun run format         # Prettier over every Markdown file, 85 columns
bun run format:check
bun run build          # fails on a dead link
bun run preview        # look at the changed pages and their diagrams
```

The site renders Mermaid, so a diagram that does not parse shows as an error
on its page.

Push to `v2`; the `docs.yml` workflow checks the format, builds and deploys.
Watch it to the end.

## Names

- Describe Chauffeur as if it exists. No sibling project is presented as
  something it depends on: an integration is its own project that calls
  Chauffeur. sourcefed is the one documented in `docs/integrations/`.
- Events are named by their owner, `<project>.<event>`, such as
  `sourcefed.event`. `tool`, `prompt`, `permission`, `response`,
  `session` and `compaction` belong to harnesses.
- The commands section is titled "CLI". A plugin's commands go under its ID,
  such as `chauffeur skills list`, in a Commands section on its page and in
  `docs/cli/README.md`'s Commands from plugins, a section that appears once
  a plugin ships.

## Pages

- **What ships**: pages describe only what ships. Planned design lives in an
  RFC sub-issue of [#43](https://github.com/wrkhse/chauffeur/issues/43),
  which opens with `**Status: RFC.** Sub-issue of #43.` A page that would
  describe unbuilt work links its issue instead.
- **Slices**: an implemented or in-progress slice has its page,
  `docs/plans/slices/slice-N.md`, listed in its `README.md`. A planned
  slice is its issue. The page opens with its status, what you can do, and
  the diagram; a complete slice's Recording follows, a few sentences on what
  the recording shows and then its GIF, so a reader sees the feature work
  before any detail. What it builds and End to end come after. Each part
  has `Files:`, a code example, `Structure:` when the slice is In progress
  or Implemented, `Commands:` when a plugin adds any, and `Verified by:`.
- **Plugins**: a plugin gets its page in `docs/plugins/` once it ships,
  with its hooks as code, its Commands and its Options. Until then its
  design is in its RFC. Integrations are in `docs/integrations/`.
- **Migration**: one page in `docs/migration/` per v1 feature that v2
  ships. Rule files move by a prompt an agent follows to rewrite them; until
  cutover that prompt is part of the cutover RFC,
  [#53](https://github.com/wrkhse/chauffeur/issues/53). There is no
  `migrate` command.
- **Module READMEs**: each Rust module folder has a `README.md` saying what
  its files hold, with one example of its use.
- A rule's `instructions` and `text` are long JSON strings; they may run
  past 85 columns.

## Recordings

Once a slice is complete and its rc is installed, record the feature working
end to end and embed it as a GIF in the slice page's Recording section, right
after the diagram, below a few sentences that say what it shows. The
recording is the reader's proof that the released rc works, so it uses the
real path: the installed `chauffeur-next`, its daemon with the real models, and
the slice's real harness or caller, never a stand-in model. Record again when
what the slice shows changes.

Drive the terminal with `termctrl` and follow the `terminal-control` skill.
[`recording.md`](recording.md) has slice 1's recording as commands to adapt.

- **Two panes, one story.** Record the harness and
  `chauffeur-next audit 0 --follow --brief` as two sessions, both 120 columns
  wide so an audit line fits on one row, then stack them with the harness on top.
  Mark both sessions at the same moments and export each with the same clips,
  so the audit line appears just as the harness reacts.
- **Only the rc acts.** Work in a scratch git repo in the temp folder whose
  `opencode.jsonc` lists `"-chauffeur"`, `"-opencode-history"` and the rc's
  adapter, so v1's plugin stays out of the session and no run remembers the
  one before it. A run where the agent gives the rule nothing to do shows
  nothing; record again until a take shows the feature.
- **Nothing private on screen.** Hide OpenCode's tabs, which show the titles
  of the user's other sessions, with
  `OPENCODE_CLI_CONFIG_CONTENT='{"tabs":{"mode":"off"}}'`, and use
  `opencode --standalone`. Look at frames from the finished GIF for keys,
  tokens and other projects before committing it.
- **Captions tell the path.** The harness clips carry captions that name each
  step, such as what the agent did and what Jev answered. The audit clips
  carry its command as the caption, which labels the pane.
- **Export the audit pane with `--include-startup`.** Without it, `termctrl`
  drops the empty frames before the first audit line, and the line shows from
  the first frame.
- **Save it as `docs/media/slice-N.gif`.** Render the stacked take in the
  scratch folder, then make the GIF with ffmpeg's palettegen and paletteuse,
  at most 1200 pixels wide, 10 frames a second and under 3 MB. Trim or cut
  the take rather than shrink it past readable. Embed it as a plain Markdown
  image; a README that shows the feature embeds the same GIF.

  ```markdown
  ![OpenCode above the audit log: the agent commits and amends](../../media/slice-1.gif)
  ```

- **Leave nothing behind.** Stop the `termctrl` sessions and any daemon you
  started, delete the OpenCode sessions in the scratch directory, and keep the
  `.termctrl` files out of the repository: they hold every byte typed and
  shown.
