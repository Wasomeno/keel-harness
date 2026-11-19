# keeltoy

A tiny collection of text and number helpers used by keel's behavioral evals.

## Layout

- `keeltoy/text.py` — whitespace, truncation and slug helpers
- `keeltoy/numbers.py` — clamping, integer parsing and averages
- `keeltoy/lists.py` — list helpers
- `keeltoy/report.py` — tabular output

## Usage

```python
from keeltoy.text import normalize_space

print(normalize_space("  a   b "))
```

Run a task's check with `python3 tests/check.py`.
