---
type: bug
context: WispX v2.0.1 nearby-text continuation fix
---

# Nearby text did not continue an existing sentence

Security impact: LOW. No security exploit path identified. Field-boundary clamping also avoids including unrelated fields in context.

## Reproduce

The reported dictation appended capitalized "System of the desires of the world." after "Basically, we're testing the". Investigation used synthetic text only, isolated Edge profiles, isolated VS Code user/extension directories, and a temporary Notepad file. No existing user app was targeted.

Live cleanup with the original instructions/prompt returned capitalized "System…" in all six contextual calls (three with a preceding space and three without). Three no-context controls also stayed capitalized. See local `artifacts/baseline/cleanup-before.json`. A header explaining the insertion marker produced lowercase output in all six contextual calls; the three no-context controls stayed capitalized (`cleanup-marker.json`). The instruction text itself was preserved.

## Isolate

The context channel expired 250 ms after hotkey start and discarded completed results after that point, even while the recording/upload continued. A native fixture deliberately delayed its worker by 350 ms; after 750 ms its valid result was `None` (`artifacts/late-red.log`).

Chromium's focused textarea, input, and contenteditable expose TextPattern. A degenerate caret selection is valid. Character-range expansion escaped each field into the rest of the document, including sibling inputs and masked password bullets. `RangeFromChild` returned the correct individual field range (`artifacts/range-probe/*.json`).

Cleanup trims its response, so relying on it for a leading join space cannot work reliably. Missing or plain context cannot identify a word boundary.

## Hypothesize

1. Slow completed results are discarded unnecessarily: reproduce using a delayed native provider worker.
2. Chromium returns unusable containers/selections: inspect focused element and ranges in fresh and warm fixtures.
3. The model does not understand the insertion marker: measure the original and minimally clarified prompts live.
4. Spacing needs deterministic joining: check existing whitespace, opening brackets/quotes, punctuation, Unicode, absent/ambiguous markers, and empty text.

## Verify

The delayed capture E2E failed on the old deadline and succeeds when completed results remain queued until transcription consumes them. Consumption uses `try_recv`, adding zero wait, and the existing single-worker guard still caps hung providers.

The native lifetime/privacy fixture reads its own explicitly supplied, read-only Edit control. It does not depend on desktop focus, and the own-window exclusion check supplies that same owned handle directly rather than reading whatever app the user is using. Browser checks exercise actual focused-element discovery and reject a switched window.

Exact Edge context is `Before the caret. [cursor]After the caret.` for all three field types. Password context is absent. Boundaries are clamped to the focused field before text is read, and a selection outside that field is rejected. A Chromium field whose range cannot be determined is omitted instead of using the whole page.

A/B cold Edge capture succeeded without explicit MSAA activation (17 ms first capture, 5 ms subsequent captures in one recorded run), so no MSAA activation workaround was added to recording. A later cold run reproduced a focused `View` container with no text on the first 3 ms capture; its next two captures returned the textarea and exact context in 5 and 4 ms. The worker now retries only pending Chromium containers for at most two seconds, stopping if the original window loses focus. Cleanup never waits for those retries. Named-field lookup in the diagnostic driver can initialize the lazy accessibility tree; setup timing is logged separately from capture timing. Natural hotkey capture exceeding 250 ms was not observed; result loss is proven by the controlled delayed-worker E2E, not claimed from setup timing.

Windows 11 Notepad supplies exact context through TextPattern. VS Code's default native-edit-context explicitly reports an inaccessible editor and can supply a placeholder rather than document text. Accessible mode exposes the real document; the fixture driver must wait for asynchronous UIA selection acknowledgment before measuring it. No VS Code-specific workaround was added to recording.

## Fix plan and validation

1. List joining/prompt failure modes, write focused tests, observe missing joining function failing, then implement the rules and clarified header.
2. Add delayed native-provider E2E, observe discarded result, then remove hotkey-relative expiry. Preserve nonblocking consumption and the single worker.
3. Assert exact external-field context and clamp ranges to the actual field; reject stale selections and container URLs.
4. Run formatting, strict Clippy, library tests, Windows build, real-endpoint E2E, and GNU cross-build before release.

The E2E substitutes synthesized audio and fixed endpoint prompts for physical-microphone dictation. The user's own app and physical microphone were not exercised.

## Resolution

Completed-result lifetime, field boundaries, marker semantics, and word joining are fixed. Validation results and release/install evidence are recorded in the PR and final task report.
