# Trace fixtures

These files are synthetic and contain no benchmark task answers, hidden tests or
real agent transcripts. They exist so the schema, reporter and tagging tests can
run without Harbor, Docker, a model provider or network access.

| file | records | use |
| --- | --- | --- |
| `all_tags.jsonl` | 16 | One valid failing trace for each failure tag. |
| `valid_run.jsonl` | 6 | Two tasks with three trials each; used by CLI examples and tests. |
| `malformed/missing_field.jsonl` | 1 | Missing `verifier_result`. |
| `malformed/bad_timestamp.jsonl` | 1 | Naive timestamp with no UTC offset. |
| `malformed/unknown_tag.jsonl` | 1 | Tag outside the fixed 16-tag taxonomy. |

`all_tags.jsonl` and `valid_run.jsonl` are intentionally small. Do not put real
benchmark solutions or hidden tests in this directory.
