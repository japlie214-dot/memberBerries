# Vector score floor sweep: loopback llama-server (30 notes, 30 on-topic questions, 12 off-topic questions, 768-wide i8 cosine as memb stores it)

Backend default floor in this build: 0.25

| scores | n | min | p10 | p50 | p90 | max |
|---|---|---|---|---|---|---|
| right note for its question | 30 | 0.206 | 0.315 | 0.459 | 0.560 | 0.657 |
| best wrong note per on-topic question | 30 | 0.210 | 0.226 | 0.283 | 0.399 | 0.497 |
| all wrong notes (on-topic questions) | 870 | -0.026 | 0.059 | 0.135 | 0.227 | 0.497 |
| best note per off-topic question | 12 | 0.045 | 0.068 | 0.117 | 0.147 | 0.166 |

Lexical search alone returns 8.2 of at most 10 notes for an off-topic question on average (it has no score floor).

| floor | right note clears it | vector only: right note in top 5 | vector only: notes per on-topic question | vector only: off-topic questions with any note | vector only: notes per off-topic question | hybrid: notes per on-topic question | hybrid: recall@5 | hybrid: notes per off-topic question | hybrid: JSON bytes per on-topic hook |
|---|---|---|---|---|---|---|---|---|---|
| 0.000 | 30/30 | 1.00 | 5.0 | 12/12 | 5.0 | 5.0 | 0.93 | 5.0 | 1265 |
| 0.250 | 29/30 | 0.97 | 2.6 | 0/12 | 0.0 | 4.9 | 1.00 | 4.4 | 1238 |
| 0.420 | 17/30 | 0.57 | 0.7 | 0/12 | 0.0 | 4.8 | 0.80 | 4.4 | 1205 |
| 0.500 | 10/30 | 0.33 | 0.3 | 0/12 | 0.0 | 4.6 | 0.67 | 4.4 | 1172 |
| 0.550 | 4/30 | 0.13 | 0.1 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.600 | 2/30 | 0.07 | 0.1 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.625 | 1/30 | 0.03 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.650 | 1/30 | 0.03 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.675 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.700 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.750 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| default (0.25) | 29/30 | 0.97 | 2.6 | 0/12 | 0.0 | 4.9 | 1.00 | 4.4 | 1238 |
