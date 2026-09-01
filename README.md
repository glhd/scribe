# Scribe

Scribe is a macOS desktop companion for a Claude `planning-scribe` skill. It
owns the live session for the current Tuple call, gives Claude a visible review
stream, and renders an internal Markdown handoff as Claude edits it.

Scribe does not need a repository-local configuration file or a meaningful
process working directory. It writes nothing into a project until the user
chooses **Save As…** for a finished handoff.

## Installed-app workflow

1. Install Scribe in Applications and open it. With no call, it waits and
   detects the next Tuple call without a restart.
2. Start transcription in Tuple when wanted. Scribe never starts or restarts
   transcription. If transcription stops during a call, Scribe reports the gap.
3. On first use, choose **Install Claude integration**. This installs the
   `planning-scribe` skill and a stable CLI shim at `~/.scribe/bin/scribe`; no
   shell `PATH` edits are needed. If a user-managed `planning-scribe` skill is
   already present, Scribe backs it up beside `SKILL.md` before installing its
   managed version.
4. Start `planning-scribe` from Claude in the Git repository being planned. The
   skill attaches that repository to the active call and learns Scribe's
   internal notes path.
5. After Tuple reports that the call ended, Claude performs its final notes pass
   and finishes the session. Scribe presents **Plan ready** with **Copy** and
   native **Save As…** actions. Save As copies the internal handoff; it does not
   move it.

The Tuple call ID is the Scribe session ID. There is no active-call picker.
History is for opening recent Scribe sessions and recovering unsaved handoffs.

Tuple's CLI must be installed from Tuple Settings → Integrations → CLI Server.
Scribe checks `/usr/local/bin/tuple`, `/opt/homebrew/bin/tuple`, and then its own
`PATH`, so a Finder-launched app does not depend on a shell working directory.

## Storage and privacy

SQLite is the single operational source of truth:

```text
~/.scribe/
  scribe.db
  bin/scribe
  sessions/<tuple-call-id>/notes.md
```

The database uses WAL, transactions, schema migrations, and a busy timeout so
the GUI can read while the CLI writes. It stores session state, normalized
source events and source health, Claude chat, decision reviews, file references,
Chronicle matches, and durable per-consumer cursors. The real internal
`notes.md` is the only non-database session document because Claude edits it and
the renderer watches it.

There are no project chat, transcript, event, or notes sidecars. Scribe has no
normal raw-transcript or Claude-chat export. A finished handoff is the only
normal export, and only an explicit Save As can put it in a project. File
references retain the Git `HEAD` captured when Claude posted them and open the
current attached-repository file in PhpStorm.

The latest five complete/interrupted sessions keep their full operational,
source, and chat data; active and finalizing sessions are always retained.
Older terminal-session operational data is pruned. Scribe keeps the latest five
internal handoffs and also protects any older handoff that has never been saved
externally, surfacing it in History for Save or Delete. A content hash tracks
the last Save As, so subsequent edits make a handoff unsaved again. Cleanup only
removes Scribe-owned internal files and never touches an exported destination.

Stale active sessions become interrupted after restart and remain recoverable.

For isolated development/tests, `SCRIBE_HOME` can replace `~/.scribe`. Normal
installed use intentionally has one stable CLI-discoverable location.

## CLI contract

The installed skill uses the absolute shim path, but examples below abbreviate
it as `scribe`:

```bash
scribe session attach --repo "$PWD"
scribe session current --json
scribe tick --wait --cursor planning-scribe --timeout 30s --limit 200
scribe session finish
```

`session attach` resolves the canonical Git root and returns JSON containing the
session ID, absolute internal `notesPath`, attached repository, state, and source
health. It never changes the notes path. `tick` performs source collection,
normalization, filtering, deduplication, chronological batch ordering, and a
transactional durable cursor for the named consumer. A late source event is
delivered once with its original `occurredAt`; the skill never parses SQLite,
Tuple, or Chronicle storage itself.

Visible chat and review commands bind to the active/finalizing Scribe session:

```bash
scribe say "The job already sets **tries** to `1`." \
  --ref-heading "Decisions>Retry placement" \
  --ref-snippet "Retries live in the job"

scribe ack "I’m checking the export path."

scribe decision "Retries live in the job, not the client." \
  --id retry-placement \
  --file app/Jobs/SyncRefundsJob.php:14

scribe unlink <message-id>
scribe read [<message-id>]
```

`say` and `decision` require `--ref-heading` and `--ref-snippet` together when
using a note reference. `--file` accepts repository-relative
`path[:line[-end]]`; backticked paths are inferred. Errors are readable on
stderr and exit nonzero. CLI writes do not depend on the GUI being available.

Run `scribe --help` for the complete syntax.

## Tuple source

Scribe discovers the active call with:

```text
tuple call current --format json
```

It consumes machine-readable transcription and lifecycle records with Tuple's
durable per-call `scribe-<call-id>` cursor. The initial read catches up backlog,
processes serialize through a per-call lock, and restarts do not create gaps or
repeats. Speech occurrence time is the spoken/start time, not transcription
completion. Tuple's explicit `call_ended` moves Scribe to `finalizing`;
`recording_ended` only reports that transcription stopped.

## Optional Chronicle source

Scribe implements the Chronicle schema-1 wire contract documented in the
[authoritative Scribe integration document](https://ampcode.com/user-content/attachments/9f979b70d160dbf20495310a44bf9b582071b3128613b806cde810d7d23852bd-scribe-integration.md).
It reads the atomic `sessions.json` registry and ignores `sessions.json.lock`.
It never looks for `current.json`.

Scribe resolves the Chronicle root in this order:

1. the Chronicle folder explicitly selected in Scribe and persisted in
   `scribe.db`;
2. `CHRONICLE_HOME` inherited by the Scribe process;
3. `~/.chronicle`.

The default is detected without prompting. **Choose Chronicle folder** appears
when no registry is found and remains available under Sources for a deliberate
override. A Finder-launched Scribe cannot inspect PhpStorm's
`-Dchronicle.home`, and it may not inherit a shell-only `CHRONICLE_HOME`; select
the matching root once in that situation. Once the root is known,
`sessions.json` supplies absolute log paths.

After the planning skill attaches a repository, Scribe matches its canonical
Git root against every `session.repositories[].root`, then prefers active and
time-overlapping sessions. Equally good matches require explicit selection.
Chronicle owns the `active`, `completed`, and `interrupted` states, including
stale PID/heartbeat demotion.

Scribe safely tails the selected absolute UTF-8 append-only JSONL log, defers a
possibly truncated final line, and treats malformed complete records as source
errors. It validates schema/version, IDs, gapless per-session sequence,
millisecond UTC timestamps, event data, and path rules; deduplicates by event ID
and source sequence; imports normalized records into SQLite; and merges by
`occurredAt`, never append order. `redacted: true` marks selected/snippet text as
untrusted while preserving accurate paths and ranges. `audio_transcription` is
rejected because Chronicle never emits it in Scribe mode.

Chronicle owns and prunes its registry/logs. Scribe never modifies or deletes
them; Scribe retention applies only to imported SQLite records.

## Development and verification

```bash
npm install
npm run tauri dev
```

Build the frontend with `npm run build` and the app with
`npm run tauri build`. Rust checks live under `src-tauri`:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
```

Release builds check GitHub releases through Tauri's updater. Creating releases,
tags, and updater artifacts is intentionally separate from this architecture.

## Compatibility

The old cwd-driven `.scribe.json`, `SCRIBE_CONFIG`, `SCRIBE_NOTES`, and
repository Markdown/JSONL sidecar workflow is intentionally removed. Existing
project sidecars are left untouched but are not imported. They cannot safely
represent one installed app session shared by concurrent GUI and CLI processes;
SQLite and the internal handoff replace that ownership model.
