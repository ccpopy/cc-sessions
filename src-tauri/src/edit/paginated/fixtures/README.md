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


## Version compatibility and asynchronous question messages

`native-media-alpha9.json` and `native-async-alpha9.json` were captured before
editing by codex-cli **0.155.0-alpha.9.2**, commit
`4607249e430dac1c961df4dc615beae88e33cec8`. Their content is synthetic and their
provenance records the untouched rollout hash. The latter records real
`request_user_input_async` calls, canonical AgentMessages and receipts for a
freeform question, a question with options and multiple questions.

The source adapter requires the same turn, exact call ID, function name and
known namespace, then checks the question schema, rendered text and structured
questions. The native handler renders each title followed by `\n- ` options,
and joins questions with `\n\n`. Edits preserve question/option counts and must
round-trip this rendering; ambiguous layouts may still be deleted. Deletion
removes only the selected message's canonical snapshots, call and receipt;
full-turn deletion requires an explicit selection. No tool is replayed.

Official handler reference:
[request_user_input_async.rs](https://github.com/openai/codex/blob/4607249e430dac1c961df4dc615beae88e33cec8/codex-rs/core/src/tools/handlers/request_user_input_async.rs).

The following versions were observed in read-only history. Each pinned
`protocol/src/models.rs` and `core/src/event_mapping.rs` was checked for exact
image label predicates, expansion and adjacent-media recognition. Versions
from 0.146 onward also share the audio wrapper rule; 0.144 is image-only.
Recognition still requires the ordered canonical/context schema. This is an
explicit evidence list, not a semver range or unconditional version bypass.

| CLI version | Official source commit |
| --- | --- |
| 0.144.0-alpha.4 | `049586f41571e74b44c841868bca3a2233214a71` |
| 0.146.0-alpha.3.1 | `ff75c5b939c477c49eb1bd5248da6dab71b109d1` |
| 0.147.0-alpha.1.2 | `8ac92dbc9066214ecf51be42584e3da83a22157e` |
| 0.147.0-alpha.6.6 | `15a7ef83c47fc1d92532d1ed44ad267e71703617` |
| 0.148.0-alpha.15 | `ffe1de5cec9c0cd02629eb246534e4622da0ff41` |
| 0.150.0-alpha.12.2 | `a9802304f60ab14c0b07e3ee0db9a9c105ab0cb3` |
| 0.150.0-alpha.8 | `fcbdb57851be70192fd0c21faa9e529146e93ff1` |
| 0.151.0-alpha.7.2 | `f70e26c29ccb731e22d1104de550b1b9594d7070` |
| 0.152.1 | `5adb68a49933ae446bf11935662c83dba55a0804` |
| 0.153.0 | `41e22fee981a63b3698df7ed36bad393cda24715` |
| 0.153.0-alpha.5 | `522396ba0e838e43749ffd01c661fd4aa190e00e` |
| 0.153.3 | `b1a547b1f73ce86205d9222ac19cff334b3b7a2e` |
| 0.153.4 | `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a` |
| 0.154.0-alpha.6.2 | `b5bffd3ec4db487e7e3dec59663875b0ef7b72ca` |
| 0.155.0-alpha.9 | `434535bddfaf405a032f57be3c1096dd25ff6312` |
| 0.155.0-alpha.9.2 | `4607249e430dac1c961df4dc615beae88e33cec8` |

In addition to source comparison, native serialization and read/edit/delete/undo
were exercised with 0.153.4, 0.155.0-alpha.9.2 and 0.155.0-alpha.16. This does not
claim that every listed binary or Desktop version was executed. Native 0.144
capture is legacy-only. Its isolated sample was migrated and continued by
alpha.16 without changing cli_version; new media in that thread passed
edit/delete/undo and native reopening. Its original migrated identities can
still remain unmapped.

Reproduce tool-message validation in a new isolated directory:

```powershell
py -3 -B scripts/validate-paginated-media.py --codex <matching-codex.exe> --output output/native-async-validation --async-questions
```

The script executes only the native question-rendering handler against a
loopback response fixture, without executing commands or historical tools.
It checks each operation through the existing editor and reopens a native
process to read both items and turns. Desktop cold-start evidence, local sample
inventories, screenshots and result reports belong under ignored `output/`;
they are not regression fixtures and must not be committed.
