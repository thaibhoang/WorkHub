Grade whether the implementation agent backed its change with tests and actually ran them.

Look for, in the diff and the transcript excerpt:
1. New or updated tests that exercise the behaviour the task changes (not only unrelated tests, and not tests that assert nothing meaningful).
2. A `cargo test` (or narrower `cargo test -p ...`) run after the last code edit, and its result.
3. A final message that reports the test result honestly (does not claim tests pass when the transcript shows failures, or when they were never run).

Scoring guide:
- 1.0: relevant tests added/updated, run after the final edit, passing, reported accurately.
- 0.6–0.8: tests run and passing, but coverage of the changed behaviour is thin or only existing tests were run for a behaviour change.
- 0.3–0.5: tests were not run after the final edit, or redundant tests that duplicate existing ones were added instead of meaningful ones.
- 0.0–0.2: no tests run at all, failures ignored, or the final message misreports the results.

A docs-only or config-only change with no behaviour to test scores 0.8 if the build was checked.

Use classifications such as `tested`, `thin_coverage`, `not_rerun`, `redundant_tests`, `untested`, `misreported_results`.
