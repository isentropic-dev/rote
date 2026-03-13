# Design

This document captures the durable design of `rote`.
It is not a user guide.
The README explains what `rote` does and how to run it.
This file explains the structure, invariants, and decisions that shape the implementation.

## Product Shape

`rote` is a terminal application that automates repetitive web data entry.
The user demonstrates a task once against a web form, and `rote` replays the same pattern across the remaining rows of tabular data.

The primary interaction model is a single session: load data, connect to a browser, demonstrate one row, then replay.
Workflow export exists to carry learning across sessions, but saved workflows are an output of the session, not a prerequisite for it.

## Design Invariants

These are the constraints the design is built around.
Changes that violate them should be treated as design changes, not incidental implementation details.

### One binary, local-first

`rote` is distributed as a single binary.
It does not depend on a hosted service, browser extension, or external runtime beyond a supported browser.

### Show, don't script

The user teaches `rote` by doing the task, not by writing selectors, rules, or code.
Configuration should emerge from observation where possible.
When observation is insufficient, `rote` should ask a focused question at the point of ambiguity.

### One session from data to done

The default path is immediate execution.
A user with data in hand should be able to start a session, teach one row, and continue directly into playback.
Intermediate artifacts are optional.

### Playback is a confidence gradient

Automation is not just on or off.
Users need to move between manual control and autonomous replay as confidence changes.
Playback speed is therefore part of the core model.

### User intervention is meaningful input

When automation encounters ambiguity or failure, user action should be treated as information.
The design aims toward adaptive training, where intervention can refine the learned workflow instead of being discarded.

### Durable artifacts are readable

Workflows are JSON.
They are intended to be inspectable, versionable, and editable outside the TUI.
Opaque binary formats are out of scope.

## Architecture

```
┌──────────────────────────────────────────────────┐
│                   rote binary                    │
│                                                  │
│  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
│  │   CLI    │  │   TUI    │  │ Training Core  │  │
│  │  (clap)  │  │(ratatui) │  │(state machine) │  │
│  └────┬─────┘  └────┬─────┘  └───────┬───────┘  │
│       │              │                │          │
│       │              └────────────────┘          │
│       │              commands / events           │
│       │                       │                  │
│  ┌────┴───────────────────────┴───────────────┐  │
│  │                 CDP Layer                   │  │
│  │    (Chrome DevTools Protocol over WS)       │  │
│  └────────────────────┬───────────────────────┘  │
│                       │                          │
└───────────────────────┼──────────────────────────┘
                        │ WebSocket
                 ┌──────┴──────┐
                 │ Chrome/Edge │
                 └─────────────┘
```

### Separation of concerns

The architectural center is the training core.
It owns session state and domain transitions.
The TUI renders state and turns user input into commands.
The CDP layer observes and manipulates the browser.

That separation exists so the domain model is not buried inside terminal code or browser plumbing.
A different frontend should be able to drive the same core.
Browser implementation details should not leak into session logic.

### Training core

The training core is a state machine.
It accepts commands and emits events.
It owns:

- current row
- captured steps
- column bindings
- playback speed
- workflow state
- prompts triggered by ambiguity or failure

This gives `rote` an explicit transition model.
State changes can be tested without a terminal or live browser.
It also creates a clean seam between "what happened" and "how it is shown."

### TUI

The TUI is the only frontend in v1.
It is responsible for:

- presenting data and progress
- exposing playback controls
- surfacing prompts and failures
- mapping keypresses to training and playback commands

The TUI should stay thin.
If business rules start accumulating in rendering code, the boundary has drifted.

### CDP layer

The browser integration layer talks to Chrome or Edge via the Chrome DevTools Protocol.
It is responsible for:

- finding and launching a supported browser
- connecting over WebSocket
- injecting recording logic
- locating elements and executing replay actions
- gathering DOM state for verification and future wait conditions

The current design uses JS injection for recording, and CDP commands plus evaluation for replay.
A future native-input mode can be added without changing the higher-level architecture.

## Core Domain Concepts

### Workflow

A workflow is the artifact learned from demonstration.
It bridges training and playback, and eventually bridges one session to the next.

A workflow contains:

- ordered steps
- column bindings
- selectors
- wait or verification conditions
- behavior for ambiguous cases such as empty cells
- enough metadata to replay against new data

### Selectors

Elements are modeled as a list of resolution strategies, not a single canonical selector.
That choice is fundamental.
Web UIs change, and different selector strategies fail differently.

```rust
enum Resolution {
    Id(String),
    Css(String),
    XPath(String),
    TextContent(String),
}

struct Selector {
    strategies: Vec<Resolution>,
    tag: String,
}
```

During training, `rote` captures every useful strategy it can derive.
During playback, it tries them in order until one resolves.

This is the main extensibility seam for future robustness work.
Dynamic strategies can be added as new enum variants without changing the surrounding model.

### Column binding

Column binding is inferred from demonstrated input.
When the user types a value during training, `rote` compares it against unbound values in the current row.
An exact match becomes a binding.
If nothing matches, the input is treated as a literal.

This keeps training close to the user's natural behavior.
The system should infer intent from the demonstration, not require a separate labeling phase.

### Playback speed

Playback speed is part of the workflow execution model.
The important distinction is not animation speed.
It is how often control returns to the user.

- **Manual**: user acts, `rote` observes
- **Cell**: `rote` acts one field at a time
- **Row**: `rote` acts one row at a time
- **Auto**: `rote` continues without stopping

The model exists because trust is incremental.
Users often want supervised replay before autonomous replay.

### Teach on first encounter

Some ambiguities should be learned once, then applied consistently.
Examples include empty cells, newly populated columns, and other cases where the demonstration did not cover a future row.

The design preference is: ask at first encounter, then encode the answer into workflow behavior.
That keeps the common path simple without forcing up-front configuration for edge cases.

## Verification and waiting

Reliable replay depends on knowing when an action is complete.
A click is not enough.
A submit button is only meaningful if `rote` can observe the condition that means "continue now."

The design direction is DOM-based verification and wait conditions.
Instead of asking the user to author conditions directly, `rote` should capture them from observation:
what changed, what element mattered, and what state signaled completion.

This area is structurally important because it determines how replay becomes reliable on real sites.
It is also one of the main pieces still to be completed.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| Rust, single binary | Simple distribution and no runtime dependency stack. |
| Terminal-first UI | Keeps the product lightweight and script-adjacent without becoming a GUI app. |
| Single-session default | The common case is immediate task completion, not workflow authoring as a separate phase. |
| Training core as state machine | Keeps domain logic testable and separate from rendering. |
| Browser control through CDP | Gives enough visibility and control without requiring an extension. |
| Multi-strategy selectors | Makes selector resolution extensible and more robust than a single selector string. |
| Automatic column binding | Lets users teach by doing rather than labeling. |
| Teach-on-first-encounter | Handles edge cases incrementally instead of front-loading configuration. |
| JSON workflows | Keeps durable artifacts readable and editable. |
| Always launch a browser | Simpler than asking users to prepare a debug-enabled browser session. |

## Non-goals for v1

The following are intentionally out of scope for the first version:

- GUI application
- browser extension
- direct Excel or Google Sheets integrations
- branching workflows based on data values
- workflow editing inside the TUI
- support beyond Chrome and Edge
- persistent browser profiles across sessions
- automatic dynamic selector pattern detection

## Relationship to other docs

- `README.md` explains the product and usage
- the issue tracker captures planned work and open problems
- release and contribution mechanics belong in their own docs as needed
