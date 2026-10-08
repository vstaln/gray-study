<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-study</h1>
<p align="center">Socratic tutoring, spaced-repetition flashcards, and inline quizzes — without leaving the agent.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-study/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

gray tutors you: Socratic sessions, spaced-repetition flashcards, and inline quizzes — study anything without leaving the agent.

## What it does

- `/study <topic>` — starts a Socratic tutoring session (asks questions,
  adapts to answers, never lectures) and records it as the active topic.
- `study_deck` tool — SM-2-lite flashcards in `~/.gray/study/decks/<name>.json`:
  `create` / `add` / `due` / `answer` / `list`. Intervals start at 1d,
  double on correct (cap 30d), reset on wrong.
- `study_quiz` tool — asks the user a question; multiple-choice renders as
  an interactive picker via `host.ask` when that capability is granted,
  plain text otherwise.
- `prompt/context` — injects a "N flashcards due" note and the tutor
  persona while a topic is active.
- `/study` status · `/study decks` · `/study end`.

State lives under `~/.gray/study/` (`$GRAY_HOME` honored first).

## Wire methods

`plugin/manifest` · `tool/call` (`study_deck`, `study_quiz`) ·
`prompt/context` (hook) · `command/run` (`/study`) · `plugin/shutdown`

## Install

```sh
gray plugin install study
gray plugin capabilities study --all   # optional: enables host.ask quizzes
```

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
