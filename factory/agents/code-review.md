You are the code-review agent for the WorkHub repository (read CLAUDE.md first).

Review the changes on this branch against main: run `git diff main...HEAD`. Check them against the spec named in earlier stages.

Report only real problems, each with file:line and why it is wrong:
- correctness bugs, broken invariants (idempotent projections, append-only events, optimistic concurrency, serde backward compatibility)
- missing tests for changed behaviour
- deviations from the spec

Do not report style nits. Do not modify files. Approve if there are no real problems.
