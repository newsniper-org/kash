---
name: 무한루프 의심 시 테스트를 하나씩 돌리기
description: hang/무한루프 의심되는 변경 후 `cargo test`를 일괄 실행하지 말고 개별 테스트별로 실행해 어느 케이스가 hang인지 좁힐 것
type: feedback
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
hang/무한루프가 의심되는 변경(특히 spawn / pipe / IO loop 류) 직후엔 `cargo test`로 모든 테스트를 한 번에 돌리지 말 것.

**Why:** 한 케이스가 deadlock하면 전체 실행이 멈춰 디버깅 비용이 큼. 사용자 시간 + CPU도 낭비. 과거 두 번 실제로 발생.

**How to apply:**
- `cargo test --list`로 테스트 이름 받고, 의심되는 새 테스트부터 개별 실행 (`cargo test --lib -- --exact NAME`).
- 각 실행에 `timeout` 래퍼 + `--test-threads=1`로 격리.
- 새로 추가한 테스트가 한 번씩 통과한 다음에야 일괄 실행.
