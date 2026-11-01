---
name: efc
description: Call a program through an efc/1 file channel (a shared folder with channel.json and requests/) when it is unreachable over the network, e.g. the agent runs in a VM.
---

# efc — calling a program through a file channel

The program (server) and the agent (client) share only a folder. The client drops a request file, the server
claims it, works, and writes a result. The `efc` binary does all of this; never write channel files by hand.

## Find the channel

```
efc list                                    # walks up from cwd, reads .efc-channels.json, EFC_PATH
efc --channel <folder|name> help            # what the server can do
efc --channel <folder|name> diag            # pulse, queue, who waits for a human, trash, what to do
```

If `efc` is not on PATH, look for it in the channel folder (`<channel>/efc`, `<channel>\efc.exe`) — servers may
put it there — or ask the user where the build is. If `channel.json` lists `clients[].sha256`, compare it with
`sha256sum` before the first run. A binary on a mounted folder may not be executable in place: copy it to a local
folder first (`cp <channel>/efc ~/efc && chmod +x ~/efc`).

## Call

```
efc --channel X call <command> [args…]      # everything after the command name goes to the command as is
efc --channel X --json call <command> …     # full result as JSON (data, artifacts, reason)
efc --channel X --params '{"k":"v"}' call <command>
efc --channel X batch steps.json            # several commands in one call: {"batch":[{"command":…,"args":[…]}],"onError":"stop"}
```

The exit code is the call's code: 0 ok, 1 failed, 2 bad arguments, 3 no such command, 4 crashed or server
restarted, 5 time is up (the call may still run — see below), 6 busy, 7 cancelled, 8 denied (no rights or the
human did not confirm), 10 no server, 11 several channels match, 13 server silent. Branch on `reason` in
`--json` output, not on the error text. Paths after `→` are artifacts already mapped to local paths — open them.

## Long commands

Your tool call is time-limited; a build or test run may be longer. Never block past the limit:

```
id=$(efc --channel X --no-wait call test)   # prints the id as soon as the server accepted the call (code 0)
efc --channel X --max 170 wait "$id"        # code 5 = still running: just call wait again
efc --channel X cancel "$id"                # if it is no longer needed
```

A call accepted by the server is never lost: its result waits in the channel until it expires.

## When things are wrong

- Code 10 or 13: the server is not running or is frozen. `efc` prints what the human must start — tell the
  user exactly that. You cannot start the server from the sandbox.
- `diag` shows `ЖДЁТ: …` for a call: the server waits for a human (confirmation on its side). Tell the user
  what to confirm; do not retry the call.
- Code 8 with `reason: no-approval`: the human declined or nobody was at the server's terminal.

## Debugging

```
efc --channel X diag                       # first: pulse, queue, waiting calls, trash, what the human must do
efc --channel X trace --id <id>            # what both sides did with one call (needs tracing on)
efc --trace --channel X call <command> …   # trace this client's side into <channel>/trace/
efc --channel X bench -n 20                # where the time goes: server pickup, work, delivery
```

The trace is a shared log: if the server runs with `--trace` (or `EFC_TRACE=1`), you can read from the sandbox
what happened on the host — claims, retries with OS error codes, failed deletes, late pulses. `efc selftest`
checks the filesystem of the machine it runs on; it is meant for the server's machine, run it only if asked.

## Deleting files and the trash

The sandbox may forbid deleting files in mounted folders. `efc` handles it: it tries to delete what it has
read and, if that is refused, moves it to `<channel>/trash/`; the server empties the trash. Rules:

- Ask the user **once** for the right to delete in the channel's folder if your environment has a tool for
  that (a delete-permission request). If granted, `efc` deletes directly; if declined, it keeps using the trash —
  both are fine.
- `efc trash --clean` (the standard `trash.clean` command) may be called at any time **without asking** —
  it removes only files inside `trash/`. `efc` also sends it by itself when the trash has more than 50 files.
- Set `EFC_NO_DELETE=1` when you know deleting is forbidden, to skip the failing attempt.
- Never delete or edit files in `requests/`, `active/`, `done/`, `cancel/` by hand.

## Do not

- Do not write `requests/*.json` yourself — ids, atomic writes and withdrawal are `efc`'s job.
- Do not poll the channel folder with your own loops — use `wait`.
- Do not run `efc serve` or `efc check` unless the user asked: `serve` exposes programs, `check` is for server
  authors.
