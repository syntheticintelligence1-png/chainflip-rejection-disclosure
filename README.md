# chainflip-rejection-disclosure

Real-runtime PoC + report for a deposit-channel bricking bug in the Chainflip State Chain
(`cf-ingress-egress` rejection refund path), including an on-demand `gas_budget` trigger.

- `REPORT.md` — full writeup, PoC source, and byte-for-byte test output.
- `poc/rejection_brick.rs` — the `cf-integration-tests` module (4 tests, all passing).
- `poc/full_test_output.txt` — captured run.

## Run

Clone `chainflip-io/chainflip-backend` at master `7fd535a`, then:

```
cp poc/rejection_brick.rs state-chain/cf-integration-tests/src/
# add `mod rejection_brick;` to state-chain/cf-integration-tests/src/lib.rs
cargo test -p cf-integration-tests rejection_brick -- --nocapture --test-threads=1
```
