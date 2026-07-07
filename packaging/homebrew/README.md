# Homebrew Formula Templates

These files are local templates for future Memra Homebrew distribution. They are checked in only as release-prep artifacts; do not publish or push a tap from this repository until a maintainer explicitly approves the release.

- `memra.rb` — stable channel
- `memra@next.rb` — next channel template

Before any future publication:

1. Build release archives for supported targets.
2. Fill version and SHA256 values.
3. Run `brew install --formula ./packaging/homebrew/memra.rb`.
4. Run `memra --version` and `memra doctor --project test-install`.
5. Re-run the repository redaction audit.

The current task prepares metadata only. It does not publish a tap and does not upload release artifacts.
