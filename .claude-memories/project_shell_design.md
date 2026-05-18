---
name: New shell design project
description: User is designing a new POSIX-compliant shell; ksh93u+m grammar as base, with zsh-unique/superior features ported on top
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
사용자는 새로운 쉘 언어 **kash**를 설계 중이다 (작업 디렉토리: /home/ybi/kash).

**이름 의도**: "Korn Again SHell" — bash("Bourne Again SHell")의 형식을 차용해 ksh의 계승임을 직접 표명. ksh93u+m 문법 baseline 선택과 정합.

**제약 우선순위 (강한 순)**
1. POSIX-compliance — 가장 강한 요구사항. POSIX sh 스크립트는 POSIX 의미론대로 동작해야 함.
2. ksh93u+m **문법**을 베이스로 채택 (주의: 소스코드가 아닌 문법만 — 즉, AT&T ksh93의 구현/라이선스와는 무관).
3. zsh 고유 기능 또는 zsh가 더 앞서나가는 기능들을 위 베이스 위에 port.

**암묵적 제약**
- 초기 대화에서 "bash, ksh93u+m, zsh의 common strict superset"이라는 표현이 등장했으나, 의미론 충돌(단어 분할 기본값, 배열 인덱싱, `local` 스코프 규칙 등) 때문에 문자 그대로의 strict superset은 불가능함을 사용자에게 설명했고, ksh93 베이스 + zsh 포팅 방향으로 좁혀짐.
- bash 호환은 명시적으로 다시 다루지 않았음 — ksh93 문법이 bash 문법의 대부분을 덮으므로 자연스러운 호환이 기대되지만, `local` 스코프 / `mapfile` / `${var,,}` 등 bash 고유 항목은 별도 결정 필요.

**Why:** 셸 언어 설계는 의미론 충돌이 본질적이라, 사용자가 "어느 쉘의 어느 의미론을 정전(canonical)으로 채택할지" 명확한 우선순위를 정해 두지 않으면 모든 설계 결정이 표류함. 위 3단계 우선순위는 그 분기점에서의 tiebreaker.

**How to apply:**
- 설계 논의에서 충돌이 발견되면 위 우선순위 순으로 결정 — POSIX > ksh93 문법 > zsh 기능.
- 사용자가 다시 이 프로젝트를 꺼낼 때, "ksh93 grammar + zsh feature port, POSIX 최우선"이라는 큰 그림을 전제로 대화를 이어갈 것.
- 구체적인 문법/의미론 결정 사항이 굳어지면 별도 메모리 파일(`project_shell_<topic>.md` 등)로 분리해서 누적할 것 — 이 파일은 큰 방향만 유지.
- 현재 단계: 설계 논의 초기. 구현/코드는 아직 시작 안 됨. /home/ybi/kash 디렉토리는 현재 비어 있는 상태로 추정.
