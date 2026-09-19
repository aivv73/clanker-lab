# Issue tracker: GitHub

Tasks and specifications are stored in GitHub Issues in the
aivv73/clanker-lab repository. Use the gh CLI from the working copy.

## Operations

- Create: gh issue create --title "..." --body-file <file>
- Read: gh issue view <number> --json title,body,labels,comments
- List: gh issue list --state open --json number,title,body,labels
- Comment: gh issue comment <number> --body-file <file>
- Add a label: gh issue edit <number> --add-label "<label>"
- Remove a label: gh issue edit <number> --remove-label "<label>"
- Close: gh issue close <number>

For multiline text, use a file containing actual newline characters.
Triage labels are defined in docs/agents/triage-labels.md.

“Publish to the issue tracker” means create a GitHub issue.
“Fetch the relevant ticket” means read the issue and its comments.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Wayfinding

For /wayfinder operations, use the Wayfinding operations section
in paclang/docs/agents/issue-tracker.md.
