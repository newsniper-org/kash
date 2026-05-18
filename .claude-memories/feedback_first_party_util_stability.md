---
name: First-party utility CLI naming/interface stability rule
description: kash가 자체 제공하는 utility CLI들(tcp-connect, udp-send 등)의 naming/interface는 임의로 바꾸지 말 것 — 정의 시점부터 stable contract
type: feedback
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 프로젝트는 *언어*뿐 아니라 *first-party utility CLIs* (예: `tcp-connect`, `udp-send`, 향후 더 추가될 수 있음)도 제공한다. 이 utility들의 naming 과 interface convention은 **stable contract**로 다뤄야 함 — 정의된 이후로는 임의로 변경하지 말 것.

**Why:** 사용자가 명시적으로 강조 ("이 유틸리티 CLI들의 naming 및 interface convention들은 절대로 임의대로 violate해서는 안 됨!"). 이유 (추론):
- 이들은 transpiler가 bash 패턴 (`/dev/tcp/...`)을 매핑하는 *대상*이라, 매핑 안정성이 곧 transpile된 스크립트의 안정성.
- POSIX style utility는 한번 배포되면 수십 년 살아남는 게 정상 — 임의 변경은 ecosystem 손상.
- 새 쉘이 자체 utility를 가진다는 결정 자체가 "장기간 안정 contract"의 선언.

**How to apply:**
- 새 first-party utility 설계 시 *반드시* 사용자와 interface (이름, flag, exit code, stdin/stdout 의미) 확정 받고 commit. 임시 가정으로 시작하지 말 것.
- 기존 utility의 interface 변경이 필요해 보이면 *반드시* 사용자에게 물어보고 진행. "이 flag를 더 일관되게 바꾸자" 같은 자율적 제안은 push back 받을 가능성 큼.
- 이 원칙은 향후 추가될 모든 first-party utility 카테고리 (네트워크 외에 JSON, YAML, HTTP, 병렬 실행, try-catch 등)에 동일 적용.
- naming convention (확정): `<도메인>-<동작>` kebab-case.
- **Mode-independence (확정)**: 모든 first-party utility는 base 모드와 postfix를 불문하고 가용. 새 utility 모드 가용성 표를 만들 때 "all modes ✓" 패턴 일관 유지. 모드별 차등을 두지 말 것.
