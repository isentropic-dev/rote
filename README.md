# rote

Record once, replay forever.

Run `rote`, show it how to fill out a web form once, and it replays the pattern for every remaining row.

No scripting. No configuration. No browser extensions. Just rote memorization.

## How it works

1. Run `rote` with your data (see [Usage](#usage) for options).
2. Navigate to the website containing your form in the browser `rote` opens.
3. Fill out the form once.
   `rote` watches, captures your actions, and maps columns to input fields.
4. When the first row is done, `rote` replays the pattern for every remaining row.

You control the pace of playback.
Start with one field at a time, move to one row at a time, then let it run on its own.
Speed up or slow down as confidence warrants.

## Install

### From a release

Download the latest binary for your platform from [Releases](https://github.com/isentropic-dev/rote/releases).

### From source

```
cargo install --git https://github.com/isentropic-dev/rote
```

Requires [Rust](https://rustup.rs/) and Chrome or Edge.

## Usage

```
rote --clipboard       # reads data from clipboard
rote --data file.tsv   # reads data from a file
```

`rote` launches a TUI and opens a browser. Follow the prompts.

### Playback speeds

Switch speeds at any time during a session:

| Key | Speed      | Behavior                                      |
|-----|------------|-----------------------------------------------|
| `1` | **Manual** | You do everything. `rote` watches and learns. |
| `2` | **Cell**   | `rote` fills one field, then waits.           |
| `3` | **Row**    | `rote` fills one row, then waits.             |
| `4` | **Auto**   | `rote` runs all remaining rows without stopping. |

`Space` toggles between Manual and Auto.
`Enter` advances past confirmation gates.

### Error handling

When a step fails, `rote` pauses and asks what to do: skip the row, retry it, or stop.

## Requirements

- Chrome or Edge (`rote` uses the Chrome DevTools Protocol)
- A terminal that supports alternate screen (most do)

## Status

`rote` is in active development.
The core train-to-play loop works end-to-end.
See the [tracking issue](https://github.com/isentropic-dev/rote/issues/1) for what's planned.
