---
name: planning-scribe
description: Maintains a live planning handoff from a Tuple call in Scribe. Use during repository planning calls when Scribe is open.
---

# Planning Scribe

<!-- installed-by-scribe -->

Use Scribe's session API as the only call timeline and chat interface. Do not
look for `.scribe.json`, inspect Scribe's database, parse Tuple or Chronicle
storage, or create notes/chat/event sidecars in the repository.

The Scribe command is `{{SCRIBE_BIN}}`. Always quote this path.

## Attach

From the repository being planned, run:

```sh
"{{SCRIBE_BIN}}" session attach --repo "$PWD"
```

Read the JSON response. It contains the Tuple call ID, absolute internal
`notesPath`, session state, attached repository, and source health. Edit only
that internal Markdown document for the handoff. Do not copy it into the
repository; the user chooses a destination later with Scribe's Save As action.

## Follow the call

Repeatedly ask Scribe for the next durable, deduplicated chronological batch:

```sh
"{{SCRIBE_BIN}}" tick --wait --cursor planning-scribe --timeout 30s --limit 200
```

Each JSON response includes `events`, `sourceHealth`, `sessionState`, and
`hasMore`. If `hasMore` is true, tick again immediately. Otherwise tick with
`--wait`. Use event `occurredAt` for chronology even when a late event arrives.
Treat stopped/error source health as a possible gap and make uncertainty clear
in the notes. Scribe never restarts Tuple transcription. Chronicle events are
already normalized from its schema-1 wire contract. When a Chronicle payload
has `redacted: true` or an untrusted `contentTrust`, do not rely on its selected
text or snippet; use the accurate path and line range to read the attached
repository file instead.

Keep the internal handoff useful throughout the call: context, requirements,
decisions and rationale, open questions, implementation plan, affected files,
and verification. Preserve exact repository-relative file paths.

Use Scribe's visible chat when useful:

```sh
"{{SCRIBE_BIN}}" say "Short review note"
"{{SCRIBE_BIN}}" ack "Short progress acknowledgement"
"{{SCRIBE_BIN}}" decision "Decision needing approval" --id stable-decision-id
```

Add `--file path[:line[-end]]` for file references. `say` and `decision` may
also use `--ref-heading` plus `--ref-snippet` to point into the handoff.

## Finish

The Tuple `call_ended` event moves the session to `finalizing`. Perform one
final notes pass, resolve any remaining Scribe decision/reference events, then
run:

```sh
"{{SCRIBE_BIN}}" session finish
```

Do not finish while the session is active. Finishing tells the app to present
Plan ready with Copy and Save As; it does not write anything to the repository.
