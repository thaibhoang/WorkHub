# New task dialog: keep "Create task" reachable with long Details

Size: XS (1 file).

## Goal
However long Details is, and on short or mobile screens, the "Create task" button in the "New task" dialog can be reached and clicked.

## Non-goals
- No change to `ui/textarea.tsx` or `ui/dialog.tsx`.
- No change to other dialogs (`benchmarks.tsx`, `proposals.tsx`) or other textareas.
- No sticky footer, no layout redesign, no behaviour change.

## Cause
`Textarea` has `field-sizing-content`, so it grows with its text without limit. `DialogContent` is fixed and centred with no max height or overflow, so the excess is off screen and cannot be scrolled to.

## Assumptions
- The fix is local to the New task dialog; other dialogs with textareas are not part of this task.
- `max-h-[50vh]` for Details, as the task suggests.
- On very short screens the whole dialog scrolls. The button may then need a scroll to come into view, and the close (X) button scrolls with the content. Both are acceptable.
- `rows={6}` stays as it is.

## Design
Only `web/src/components/NewTaskDialog.tsx`, classNames only:
- `<DialogContent>`: add `max-h-[calc(100dvh-2rem)] overflow-y-auto`, keeping `sm:max-w-lg`.
- `<Textarea id="body">`: add `className="max-h-[50vh] overflow-y-auto"`.

No events, commands, projections, types or migrations.

## Boundaries
- Enter in Title still submits; Enter in Details still inserts a newline.
- Shared `ui/*` components, API calls and form state stay unchanged.

## Plan
1. Add the two classNames. Check: `cd web && npm install && npm run build`.

## Acceptance criteria
- `cd web && npm run build` passes.
- `git diff --stat` shows `web/src/components/NewTaskDialog.tsx` as the only source file changed.
- Manual (`cd web && npm run dev`, API from `cargo run --bin wh -- serve`): paste about 200 lines into Details. Details scrolls inside itself and "Create task" is visible and clickable.
- Manual: at a 375×667 viewport and at a short one (e.g. 800×400), the button can be reached (scrolling the dialog if needed) and the header can be scrolled back to.
- Manual: Enter in Title submits; Enter in Details adds a line.

If no browser is available to the implementer or verifier, report the manual checks as not run.
