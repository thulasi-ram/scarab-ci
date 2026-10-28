# Writing guide

How Scarab's docs, READMEs, ADRs and UI copy should read. This covers style.
[positioning.md](positioning.md) covers what we claim; this covers how we say it.

## Voice

Write like an engineer explaining something to a colleague. The model is Simon
Willison's blog: plain, concrete and specific, and calm about limits.

- Lead with what the reader can do or get, then show it: a command, a snippet,
  or what they'll see.
- Use "you" for the reader and "we" for the project.
- Short sentences, short paragraphs, active voice.
- Say why, and what we measured or tried.
- State limits as facts: "This doesn't handle X yet (see #123)." Don't
  apologise for them, and don't dramatise them.
- Dry wit is fine if it comes naturally. Never force a joke.

## Cut the filler

**No intensifiers.** Delete *actually, really, truly, genuinely, simply,
clearly, very, fully, entirely, deliberately, precisely, honestly*. If the
sentence means the same without the word, the word was filler. If it doesn't,
replace the word with the fact it stood for:

| Instead of | Write |
|---|---|
| the run stalls honestly | the run stalls, and the status page says why |
| it is genuinely different | it is different: X instead of Y |
| this is critical | name what breaks without it |

Swapping one intensifier for another is not a fix ("honestly" → "truthfully",
"genuinely" → "actually").

**As few adjectives as possible.** Prefer a number, a name or an example to an
adjective. No "modern", "powerful", "robust", "seamless", "elegant", "rich",
"blazing", "next-generation" or "enterprise-grade".

**Keep technical words that carry meaning.** "Exactly once", an "explicit
`needs` edge" and "fail-closed" say something specific, so they stay.

## Words we don't use

| Avoid | Use instead |
|---|---|
| substrate | storage layer, backend, kernel, infrastructure: say which |
| wedge | differentiator, starting point |
| load-bearing | name what depends on it ("the scheduler's hot path needs this") |
| first-class | built-in, its own (entity, API, …) |
| honest / honestly (as praise or framing) | say what is reported or admitted |
| seam, posture, story ("the auth story"), lens | interface, settings, design or plan (a defined term like *Take* is fine) |
| surface (as a verb) | show, report, appear |
| red lines, north star | known limits, goal |
| leverage, delve, crucially, "it's worth noting", "in short", "at its core" | cut them |

## Patterns to avoid

- "X, not Y" used for drama. Keep it only when Y is a real alternative the
  reader might assume.
- Bold one-line slogans in the middle of the text.
- Lists of three added for rhythm.
- Several em-dashes in one sentence or paragraph. Use commas, colons,
  parentheses or full stops.
- Rhetorical questions that you answer right away.
- Sentences that tell the reader how to feel ("this is the important part").

## Accuracy beats style

A style edit must never change a claim. Keep qualifiers such as *only*,
*never*, *not yet*, *per Attempt* and *control-plane*. Scope durability claims
to the control plane, as [positioning.md](positioning.md) requires. Capitalised
domain terms (Run, Step, Take, Attempt, Environment, Data Depot, …) come from
[CONTEXT.md](../CONTEXT.md); don't rename them in prose.

ADRs record decisions. Their Context sections describe the world when the
decision was made, so don't rewrite them to match today. If one would mislead a
reader now, add a dated status note under the Status block.

## Before you merge a large doc change

Have someone (or an agent) who didn't write it read the diff and look for two
things: claims that changed meaning, and new filler.
