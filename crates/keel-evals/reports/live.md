# keel M1 behavioral eval report

- Mode: live recording
- Model: neuralwatt/deepseek-v4.1-flash
- Budget: $10.000000 hard cap; spent $0.055688
- Prices: $0.150000 input, $0.015000 cached input, $0.600000 output per 1M tokens
- Price source: NeuralWatt listed token prices, https://neuralwatt.com/pricing, read 2026-11-19
- Result: 12 passed, 0 failed, 0 stopped by budget, 0 not run; 297069 input tokens, 18459 output tokens

| task | result | turns | tool calls | input tok | output tok | reasoning tok | stop | check | spend |
|---|---|---:|---:|---:|---:|---:|---|---|---:|
| truncate | pass | 4 | 4 | 14322 | 353 | 26 | end_turn | pass | $0.002362 |
| slugify | pass | 6 | 9 | 23757 | 1227 | 537 | end_turn | pass | $0.004303 |
| parse-int | pass | 7 | 8 | 29033 | 1929 | 896 | end_turn | pass | $0.005518 |
| mean | pass | 5 | 6 | 18515 | 683 | 135 | end_turn | pass | $0.003190 |
| clamp | pass | 4 | 4 | 14405 | 275 | 0 | end_turn | pass | $0.002328 |
| unique | pass | 7 | 9 | 27252 | 1003 | 263 | end_turn | pass | $0.004696 |
| chunk | pass | 5 | 6 | 18938 | 605 | 107 | end_turn | pass | $0.003207 |
| format-table | pass | 9 | 11 | 39886 | 7965 | 6546 | end_turn | pass | $0.010769 |
| refactor-whitespace | pass | 5 | 7 | 19306 | 603 | 69 | end_turn | pass | $0.003261 |
| rename-format-row | pass | 8 | 10 | 34114 | 1759 | 597 | end_turn | pass | $0.006179 |
| readme-slugify | pass | 6 | 7 | 22941 | 658 | 65 | end_turn | pass | $0.003839 |
| package-exports | pass | 8 | 12 | 34600 | 1399 | 316 | end_turn | pass | $0.006036 |

Replay is offline and comes from one cassette per task: the model responses are recorded, while the tool calls execute for real against a temp fixture copy. Live mode records through keel-testkit's loopback recorder and stops before the request that would cross the cap.
