# rote

You have rows of data in a spreadsheet and a web form that needs each one entered by hand.

Often it seems like the easiest thing to do is just... do it.
Copy a value, switch to the browser, paste, tab to the next field, switch back, copy, switch, paste.
For hundreds of rows, that's hours of tedious, error-prone work.

You could write a script, but now you're inspecting the page, writing selectors, debugging, and rewriting it when the site changes.
You could buy an enterprise automation platform, but that's a lot of machinery for what should be a simple task.

`rote` takes a different approach: show it how to fill out the form once and let it handle the rest.

No scripting. No configuration. No browser extensions. Just rote memorization.

## How it works

1. Run `rote` with your data (see [Usage](#usage) for options).
2. Navigate to the form in the browser `rote` opens.
3. Fill it out once for the first row.
   `rote` watches, captures your actions, and maps columns to fields.
4. When that row is done, `rote` replays the pattern for the rest.

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
rote --clipboard                 # reads data from clipboard
rote --data file.tsv             # reads data from a file
rote --data file.tsv --url …    # also navigates to URL to start training
```

Either `--clipboard` or `--data` is required.
`--url` is optional and can be combined with either.

`rote` launches a TUI and opens a browser.
It's designed to guide you through each step — training, then playback.

### Playback modes

Switch modes at any time during playback:

| Key | Mode     | Behavior                                  |
|-----|----------|-------------------------------------------|
| `1` | **Step** | Fill one field, then wait.                |
| `2` | **Walk** | Fill one row, then wait.                  |
| `3` | **Run**  | Fill all remaining rows without stopping. |

`Enter` advances to the next step or row.
`+`/`-` adjust speed multiplier (0.25×–4.0×).

### Error handling

When a step fails, `rote` pauses and asks what to do:
`s` to skip the row, `r` to retry it, or `q` to stop.

## Requirements

- Chrome or Edge (`rote` uses the Chrome DevTools Protocol)
- A terminal that supports alternate screen (most do)

## Status

`rote` is in active development.
The core train-to-play loop works end-to-end.
See the [tracking issue](https://github.com/isentropic-dev/rote/issues/1) for what's planned.
