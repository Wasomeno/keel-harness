# keel M1 behavioral eval report

- Mode: live recording
- Model: neuralwatt/deepseek-v4.1-flash
- Budget: $10.000000 hard cap; spent $0.034032
- Prices: $0.150000 input, $0.015000 cached input, $0.600000 output per 1M tokens
- Price source: NeuralWatt listed token prices, https://neuralwatt.com/pricing, read 2026-11-19
- Result: 12 passed, 0 failed, 0 stopped by budget, 0 not run; 180561 input tokens, 11482 output tokens

| task | result | turns | tool calls | input tok | output tok | reasoning tok | stop | check | spend |
|---|---|---:|---:|---:|---:|---:|---|---|---:|
| truncate | pass | 6 | 6 | 13624 | 682 | 226 | end_turn | pass | $0.002457 |
| slugify | pass | 9 | 13 | 25167 | 1294 | 391 | end_turn | pass | $0.004560 |
| parse-int | pass | 7 | 8 | 18482 | 1411 | 490 | end_turn | pass | $0.003624 |
| mean | pass | 5 | 6 | 11486 | 852 | 302 | end_turn | pass | $0.002238 |
| clamp | pass | 4 | 4 | 8802 | 307 | 14 | end_turn | pass | $0.001509 |
| unique | pass | 7 | 7 | 16654 | 666 | 101 | end_turn | pass | $0.002903 |
| chunk | pass | 6 | 7 | 14323 | 742 | 191 | end_turn | pass | $0.002599 |
| format-table | pass | 5 | 5 | 11513 | 2620 | 2133 | end_turn | pass | $0.003303 |
| refactor-whitespace | pass | 5 | 5 | 11957 | 517 | 98 | end_turn | pass | $0.002107 |
| rename-format-row | pass | 7 | 11 | 18990 | 829 | 137 | end_turn | pass | $0.003352 |
| readme-slugify | pass | 7 | 9 | 17702 | 1003 | 372 | end_turn | pass | $0.003263 |
| package-exports | pass | 5 | 9 | 11861 | 559 | 42 | end_turn | pass | $0.002117 |

Replay is offline and comes from one cassette per task: the model responses are recorded, while the tool calls execute for real against a temp fixture copy. Live mode records through keel-testkit's loopback recorder and stops before the request that would cross the cap.
