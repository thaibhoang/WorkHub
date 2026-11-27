Grade the quality of the code-review agent's verdict.

A good review:
- ends with a `VERDICT: APPROVE` or `VERDICT: REJECT` line that matches its findings
- backs every reported problem with a file:line reference and a concrete reason it is wrong (a bug, a broken invariant, a spec deviation, a missing test)
- on REJECT, gives feedback the implementation agent can act on without guessing
- does not reject over style nits, and does not approve when its own findings describe real bugs
- shows (in the transcript) that it actually read the diff (`git diff main...HEAD` or equivalent)
- ran the build, tests and acceptance-criteria checks itself and reports their results (the review stage is also the verification stage)
- labels findings Critical / Important / Nit and rejects only on Critical or Important

Scoring guide:
- 1.0: verdict consistent with well-evidenced findings, actionable feedback, diff inspected.
- 0.5–0.7: correct verdict, but evidence is vague (no file:line) or feedback is hard to act on.
- 0.0–0.4: verdict contradicts the findings, rejects over nits, approves obvious bugs, or never looked at the diff.

Use classifications such as `well_evidenced`, `vague_evidence`, `nitpicking`, `rubber_stamp`, `inconsistent_verdict`, `diff_not_read`.
