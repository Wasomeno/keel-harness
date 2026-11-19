# keel M1 behavioral eval report

- Mode: live recording
- Model: neuralwatt/deepseek-v4.1-flash
- Budget: $10.000000 hard cap; spent $0.039297
- Prices: $0.150000 input, $0.015000 cached input, $0.600000 output per 1M tokens
- Price source: NeuralWatt listed token prices, https://neuralwatt.com/pricing, read 2026-11-19
- Result: 12 passed, 0 failed, 0 stopped by budget, 0 not run; 199408 input tokens, 15539 output tokens

| task | result | turns | tool calls | input tok | output tok | reasoning tok | stop | check | spend |
|---|---|---:|---:|---:|---:|---:|---|---|---:|
| truncate | pass | 4 | 4 | 9426 | 423 | 88 | end_turn | pass | $0.001671 |
| slugify | pass | 8 | 12 | 22846 | 1712 | 789 | end_turn | pass | $0.004462 |
| parse-int | pass | 9 | 10 | 28049 | 1849 | 597 | end_turn | pass | $0.005325 |
| mean | pass | 4 | 4 | 9485 | 463 | 124 | end_turn | pass | $0.001704 |
| clamp | pass | 5 | 5 | 11923 | 399 | 45 | end_turn | pass | $0.002032 |
| unique | pass | 5 | 5 | 11757 | 656 | 241 | end_turn | pass | $0.002161 |
| chunk | pass | 7 | 9 | 19358 | 830 | 48 | end_turn | pass | $0.003407 |
| format-table | pass | 5 | 6 | 12791 | 5502 | 4895 | end_turn | pass | $0.005225 |
| refactor-whitespace | pass | 7 | 7 | 19508 | 1033 | 214 | end_turn | pass | $0.003552 |
| rename-format-row | pass | 8 | 11 | 22663 | 756 | 37 | end_turn | pass | $0.003860 |
| readme-slugify | pass | 6 | 7 | 15315 | 985 | 409 | end_turn | pass | $0.002892 |
| package-exports | pass | 6 | 10 | 16287 | 931 | 144 | end_turn | pass | $0.003006 |

Replay is offline and comes from one cassette per task: the model responses are recorded, while the tool calls execute for real against a temp fixture copy. Live mode records through keel-testkit's loopback recorder and stops before the request that would cross the cap.
