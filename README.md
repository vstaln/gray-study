# gray-study

gray tutors you: Socratic sessions, spaced-repetition flashcards, and
inline quizzes — study anything without leaving the agent.

A sidecar plugin for [gray](https://github.com/vstaln/gray), scaffolded by
[gray-account](https://github.com/vstaln/gray-account).

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
