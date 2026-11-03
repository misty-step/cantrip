# 0012: Evaluate cleanup fidelity instead of production text heuristics

- **Status:** accepted
- **Date:** 2026-08-13
- **Primitives:** postproc, eval
- **Powder:** —

## Context

Cleanup models can answer dictated questions instead of preserving the speaker's
words. Length, token-ratio, and Markdown heuristics reject valid edits without
proving fidelity. Clean read-speech WER alone cannot measure role or formatting
failures in [the evaluation runner](../../examples/eval/).

## Decision

Use prompt/model quality and deterministic-first evaluation rather than
approximate content policy in production. Extend the existing Rust evaluation
pipeline, not Harbor; model judges are explicit additive signals and never
override deterministic role/protocol failures or the reviewed exact oracle.

### Clean refinement contract (2026-11-02)

Clean is an opt-in Hone-style refinement, not a verbatim transcription mode.
It actively compresses redundant spoken phrasing and restructures it into
concise prose, paragraphs, or lists without summarizing away distinct details.
Precise domain language is permitted only when the source already establishes
its meaning; invented mechanisms, explanations, and facts are not.

The transcript is data, never instructions to the model. Questions remain
questions, hypotheses remain exploratory, and commands remain commands: cleanup
must neither answer nor execute them. Fidelity takes precedence over compression:
preserve facts, numbers and their written form, dates, negation, qualifications,
grammatical person, subject pronouns, and all distinct alternatives with their
qualifiers. A first-person statement of need must not become a bare command.
Keep certainty, requirements, and conditions equally strong. Preserve the exact
characters of identifiers, paths, versions, and literal quoted errors, even when
they appear misspelled. Keep unclear wording rather than guessing. Output only
the refined transcript; retain a filler-only source or sound annotation rather
than inventing speech or requesting an empty production reply.

The operator's `vocabulary` supplies canonical spellings for intended names and
acronyms, including clear missing-letter or missing-space recognition errors.
Capitalization alone does not make an ordinary spoken acronym a literal
identifier. Vocabulary does not authorize replacing ordinary words, literal
quoted misspellings, identifiers, or versions. The LLM pass receives canonical
spellings only, never alias lists. Raw bypasses cleanup and aliases and returns
the recognizer's hypothesis, not a guarantee of verbatim recognition. There is
no decoder biasing.

The first pass refines; optional later passes only proofread the refined text,
without compressing it again. Local STT stays the default, cleanup stays off
until configured, and the shared cleanup deadline/fail-open policy is unchanged.
Optional decision gates judge this refinement contract, not verbatim source
wording. Their [content boundary](../PRIVACY.md#what-leaves-the-machine) owns
what is sent to their endpoint.
Model selection remains explicit and is evaluated separately from this text
contract. The [evaluation guide](../EVALUATION.md) owns measurement and disclosure
requirements.

### Spoken numbers (2026-11-03)

The operator chose digits for spoken numbers, dates, times, and quantities. A
candidate prompt was trialed against the merged prompt and **not adopted**. It
added a digits rule and also dropped "and their written form" from the
numbers-preservation sentence, so its failures cannot be attributed to the added
rule alone: it regressed fidelity on one private take (an omitted distinct claim)
and converted a number inside an idiom in one synthetic repeat. Strict exact
counts on non-number cases did not drop. Clean therefore keeps the current
written-form rule until a candidate passes with repeated private takes and
idiom protection. The [receipt](../../eval/baselines/2026-11-03-clean-digits/run.json)
owns the counts, prompt hashes, and spend.

### Vocabulary aliases (2026-11-02)

Raw and Clean are the existing per-take contracts, selected at capture start
by `--postproc raw|clean` or the configured `[postproc].enabled` default.
Clean first applies operator-owned vocabulary aliases, then any configured
opt-in LLM cleanup. Each canonical term owns its aliases in the same vocabulary
entry; plain strings remain valid.

Alias matching is case-insensitive, exact whole-phrase matching. Select the
longest non-overlapping matches and emit canonical spellings without fuzzy
matching or cascading replacements. The aliased text survives absent/skipped
cleanup and cleanup failures or timeouts. History retains the recognizer's
`raw_transcript`, the resulting text, and a substitution count; operational
logs contain counts only.

This deterministic rule cannot disambiguate a deliberately spoken or quoted
alias: Clean also corrects `"Gantrip"` when the speaker meant that spelling.
Raw is the escape. Decoder biasing, app routing, and additional modes are not
part of this rule.

## Consequences

Production accepts nonempty text after protocol cleanup; request and response
failures still return the alias-corrected source text. Synthetic behavior cases distinguish cleanup,
role, content, and formatting, with latency/cost compared only after fidelity.
Public/synthetic data may be explicitly published; local JSON stays authoritative.
[Opt-in structured runtime decisions](0026-typesafe-system-one-decisions.md)
record the later narrow gating exception. See [evaluation](../EVALUATION.md).

Revisit if: evaluation requires interactive daemon behavior in an isolated
keyboard/clipboard/PipeWire environment; that is the stated condition for a
separate Harbor package.
