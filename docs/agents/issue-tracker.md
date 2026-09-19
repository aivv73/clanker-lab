# Issue tracker: GitHub

Задачи и спецификации хранятся в GitHub Issues репозитория
aivv73/clanker-lab. Используйте gh CLI из рабочей копии.

## Операции

- Создать: gh issue create --title "..." --body-file <file>
- Прочитать: gh issue view <number> --json title,body,labels,comments
- Перечислить: gh issue list --state open --json number,title,body,labels
- Комментировать: gh issue comment <number> --body-file <file>
- Добавить метку: gh issue edit <number> --add-label "<label>"
- Удалить метку: gh issue edit <number> --remove-label "<label>"
- Закрыть: gh issue close <number>

Для многострочного текста используйте файл с настоящими переводами строк.
Метки triage определены в docs/agents/triage-labels.md.

«Publish to the issue tracker» означает создать GitHub issue.
«Fetch the relevant ticket» означает прочитать issue и комментарии.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Wayfinding

Для операций /wayfinder используйте раздел Wayfinding operations
в paclang/docs/agents/issue-tracker.md.
