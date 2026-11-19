# keel M1 behavioral eval report

- Mode: live recording
- Model: neuralwatt/deepseek-v4.1-flash
- Budget: $10.000000 hard cap; spent $0.027273
- Prices: $0.150000 input, $0.015000 cached input, $0.600000 output per 1M tokens
- Price source: NeuralWatt listed token prices, https://neuralwatt.com/pricing, read 2026-11-19
- Result: 12 passed, 0 failed, 0 stopped by budget, 0 not run; 128127 input tokens, 13329 output tokens

| task | result | turns | tool calls | input tok | output tok | reasoning tok | stop | check | spend |
|---|---|---:|---:|---:|---:|---:|---|---|---:|
| truncate | pass | 4 | 4 | 6298 | 254 | 0 | end_turn | pass | $0.001102 |
| slugify | pass | 6 | 6 | 10496 | 592 | 210 | end_turn | pass | $0.001934 |
| parse-int | pass | 6 | 7 | 12023 | 1536 | 581 | end_turn | pass | $0.002729 |
| mean | pass | 4 | 4 | 6501 | 598 | 243 | end_turn | pass | $0.001337 |
| clamp | pass | 4 | 4 | 6441 | 326 | 13 | end_turn | pass | $0.001165 |
| unique | pass | 6 | 6 | 9827 | 616 | 133 | end_turn | pass | $0.001849 |
| chunk | pass | 6 | 6 | 11189 | 703 | 204 | end_turn | pass | $0.002104 |
| format-table | pass | 12 | 13 | 30036 | 6439 | 4911 | max_turns | pass | $0.008379 |
| refactor-whitespace | pass | 4 | 4 | 6812 | 354 | 21 | end_turn | pass | $0.001238 |
| rename-format-row | pass | 5 | 8 | 9370 | 623 | 70 | end_turn | pass | $0.001784 |
| readme-slugify | pass | 6 | 6 | 10156 | 488 | 23 | end_turn | pass | $0.001822 |
| package-exports | pass | 5 | 9 | 8978 | 800 | 265 | end_turn | pass | $0.001830 |

Replay is offline and comes from one cassette per task: the model responses are recorded, while the tool calls execute for real against a temp fixture copy. Live mode records through keel-testkit's loopback recorder and stops before the request that would cross the cap.
