CONTEXT CHECKPOINT COMPACTION

Write a handoff for the model that continues this session. Do not call tools or ask questions.

First output one fenced JSON block, with nothing before it:

```json
{"goal":"...","plan":[{"step":"...","status":"pending|in_progress|done|blocked"}],"decisions":[{"decision":"...","reason":"..."}],"blockers":["..."],"next_step":"..."}
```

Start from the harness-tracked memory above; an empty field inherits its previous value. Keep old facts unless contradicted. Do not invent paths, commands or results.

Then write the prose handoff: progress and changed files, decisions and why, constraints, remaining steps and verification, and critical paths, commands and errors. Keep names and values verbatim.
