# Native media regression fixture

`native-media-alpha16.json` contains verbatim canonical/context message pairs
emitted by **codex-cli 0.155.0-alpha.16**, before any CC Sessions edit. All inputs
are synthetic: a one-pixel PNG, silent WAV and fixed text. Paths point only to
the isolated capture directory; tests never open those media paths.

The full untouched rollout SHA-256, original thread/turn/item IDs and provenance
are embedded in the fixture. Tests rebuild only surrounding lifecycle records
and ordinals; the captured content blocks and source tags remain unchanged.

The original implementation failed five of eight diagnostic tests: local image
only, text plus image, multiple images, local audio, and mixed text/media. It
mistook generated `user.text` wrapper blocks for body text. Inline image/audio
and literal tag-text controls already passed.

Matching official source, tag `rust-v0.155.0-alpha.16`, commit
`0e2f848bf4a4e8d41a02d848a851ba126c09d185`:

- [Content conversion and exact tag predicates](https://github.com/openai/codex/blob/0e2f848bf4a4e8d41a02d848a851ba126c09d185/codex-rs/protocol/src/models.rs)
- [Wrapper recognition using adjacent media](https://github.com/openai/codex/blob/0e2f848bf4a4e8d41a02d848a851ba126c09d185/codex-rs/core/src/event_mapping.rs)
- [Official local-image rollout tests](https://github.com/openai/codex/blob/0e2f848bf4a4e8d41a02d848a851ba126c09d185/codex-rs/core/tests/suite/image_rollout.rs)

Reproduce with a new, nonexistent output directory:

```powershell
py -3 -B scripts/validate-paginated-media.py --codex <matching-codex.exe> --output output/native-media-validation
```

Use `--capture-only` to create untouched input before testing an older editor.
The script downloads the pinned official model catalog, enables audio for its
local fixture model, and directs Responses requests exclusively to a loopback
server returning fixed assistant text. There is no real model generation, tool
execution or credential copying. This tests actual native serialization,
projection, process reopening, edits and undo; Desktop GUI acceptance is
reported separately. Diagnostic summaries contain identities, types, source
tags, block counts, wrapper positions and first-difference offsets, not payloads.
