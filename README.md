# Scribe

Scribe is a desktop companion for the `planning-scribe` Claude skill. During a
Tuple planning call it gives Claude one visible voice, renders the live notes,
and lets the room approve or reject decisions without adding another text input.

The app is deliberately file-based. Claude writes through the `scribe` CLI,
the window watches the files, and a crashed or closed window cannot lose a
message.

## Configure a call

Create `.scribe.json` in the repository Claude is working in:

```json
{
  "call": "retry-placement"
}
```

`basePath` defaults to `docs`, producing these sidecars:

```text
docs/retry-placement.md
docs/retry-placement.chat.jsonl
docs/retry-placement.events.jsonl
```

Change the base directory without changing the app:

```json
{
  "call": "retry-placement",
  "basePath": "planning"
}
```

For an arbitrary document path, use `{"document":"path/to/call.md"}`. Paths
are relative to the config file. `SCRIBE_CONFIG` selects another config file and
`SCRIBE_NOTES` overrides configuration with an explicit markdown path. The app
and CLI must run inside the document's Git repository so file references can be
stamped and opened safely.

## Run

```bash
npm install
npm run tauri dev
```

Build the distributable app with `npm run tauri build`. The resulting `scribe`
executable is both the desktop entry point and the CLI: with no subcommand it
opens the window; with a subcommand it writes the sidecars directly.

Release builds check the latest GitHub release on startup. When a newer signed
version exists, Scribe downloads and installs it automatically, then relaunches
on macOS and Linux. The Windows installer handles its own relaunch.

## CLI

```bash
scribe say "The job already sets **tries** to `1`." \
  --ref-heading "Decisions>Retry placement" \
  --ref-snippet "Retries live in the job"

scribe ack "Chris asked me to add docx to the export formats. Working on that now."

scribe decision "Retries live in the job, not the client." \
  --id retry-placement

scribe unlink <message-id>
scribe read [<message-id>]
```

`say` and `decision` accept one document locator. Both locator options are
required together, and heading levels are separated with `>`.

File references can be passed explicitly and may include a line or range:

```bash
scribe say "`app/Jobs/SyncRefundsJob.php:14` already sets the retry count." \
  --file app/Jobs/SyncRefundsJob.php:14
```

Backticked repository-relative paths in message text are detected automatically,
so `--file` is only needed when the path is not written literally. The CLI
resolves `HEAD` and stores the full commit SHA in the message record. Any failure
is printed to stderr and exits non-zero.

## Markdown contracts

Claude owns the notes document. A decision entry uses an invisible stable ID and
a visible, greppable status line:

```markdown
### Retry placement
<!-- scribe-decision: retry-placement -->
**Chose:** Retries live in the job, not the client
**Because:** The client is shared with the sync path
**Ruled out:** Middleware — adds a layer for one call site
**Touches:** `app/Jobs/SyncRefundsJob.php:14` @a1b2c3d
**Status:** unreviewed
```

The skill changes `**Status:**` to `approved` or `rejected` after consuming the
corresponding app event. It should append the short SHA after every document file
reference. Scribe hides that suffix in the rendered pane, shows it on hover, and
opens the current file through `phpstorm://open`.

## JSONL contracts

The chat log contains one complete message object per line. New messages append;
`unlink`, `read`, and decision review atomically rewrite the file so each ID still
has exactly one current record.

```json
{"id":"retry-placement","kind":"decision","timestamp":"2026-08-31T12:00:00.000Z","text":"Retries live in the job.","reference":{"heading":["Decisions","Retry placement"],"snippet":"Retries live in the job"},"files":[{"path":"app/Jobs/SyncRefundsJob.php","line":14,"sha":"a1b2c3d..."}],"read":false,"decisionStatus":"unreviewed"}
```

The event log is append-only and uses the same timestamps as the transcript and
Chronicle logs:

```json
{"timestamp":"2026-08-31T12:01:00.000Z","type":"decision_approved","decisionId":"retry-placement"}
{"timestamp":"2026-08-31T12:02:00.000Z","type":"decision_rejected","decisionId":"retry-placement"}
{"timestamp":"2026-08-31T12:03:00.000Z","type":"reference_stale","messageId":"<message-id>","locator":{"heading":["Decisions","Retry placement"],"snippet":"Retries live in the job"}}
```

The planning skill's `tick.sh` should read `<call>.events.jsonl` incrementally,
merge these records by `timestamp` with speech and IDE events, and persist its
event cursor in the existing state directory. A `reference_stale` record calls
for `scribe unlink`; decision records call for updating the matching markdown
entry's `**Status:**` line.

## Releases

Versions are kept in `package.json`, `src-tauri/Cargo.toml`, and
`src-tauri/tauri.conf.json`. Pushing a matching `v<version>` tag runs the release
workflow for macOS (universal), Linux x86-64, and Windows x86-64. The workflow
publishes installers, signed updater bundles, and `latest.json` to the GitHub
release. It requires the repository secret `TAURI_SIGNING_PRIVATE_KEY`; Apple
signing and notarization secrets are optional, with ad-hoc signing used when
they are absent. To notarize, configure the complete set `APPLE_CERTIFICATE`,
`APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID`.
