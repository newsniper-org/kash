---
name: New shell — function scope and capture (committed)
description: 함수 정의 form별 스코프 규칙 확정 사항 — POSIX dynamic / ksh static / read-only capture list
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 함수 스코프 시스템의 확정 사항. (관련: project_shell_design.md, project_shell_modes.md)

## 함수 정의 form별 의미

| form | 스코프 | capture |
|---|---|---|
| `f() { }` | dynamic (POSIX form, caller scope 공유) | 해당 없음 |
| `function f { }` | static (caller의 local/typeset 안 보임) | 없음 |
| `function f() { }` | static (위와 동의어, 빈 capture list로 처리) | 없음 |
| `function f(a, b, ...) { }` | static + 명시된 이름만 caller에서 import | read-only by-ref |

## Capture semantics

- **By reference, read-only.**
  - 같은 변수 cell alias.
  - 함수 내 mutation 시도 시 거부 (`typeset -r`과 동일 메커니즘).
  - Mutation 거부 대상 (완전 카탈로그):
    - `x=...`, `x+=...` 할당
    - `((x = ...))`, `((x++))`, `((x--))` 산술 할당
    - `unset x`
    - `read x`, `read -r x`, (포팅 시) `mapfile -t x`
    - `let x=...`
    - `typeset x` 재선언
    - `typeset -n ref=x` 후 `ref=...` (transitive read-only 전염)
    - (포팅 시) `printf -v x ...`
    - `getopts spec x`
    - `eval "x=..."` (runtime 거부)
  - 거부 시점: 가능하면 parse-time, 안 되면 runtime.

- **Caller scope = dynamic call chain 전체 traverse.**
  - 직속 caller에 없으면 chain 위로 올라가며 찾음.
  - 어디에도 없으면 unset (`$undeclared`와 동일 처리).
  - strict 모드에서 unset → error 격상 여부 미결.

- **Attributes (`typeset -i`, `-a`, `-A` 등)는 reference 통해 자동으로 전파.**

## 설계 정당성

- Static base + opt-in dynamic capture는 leaky locals 함정과 정보 전달 verbose함을 *둘 다* 회피.
- Read-only capture 덕분에 함수 시그니처 = "외부에서 읽는 변수의 완전한 명세."
- Read vs write 권한이 문법적으로 분리됨 — capture는 read 전용, write는 nameref나 unguarded global을 통해서만 가능 (둘 다 명시적이라 reviewer가 식별 가능).
- `function f() { }`의 ksh93 footgun 제거 — 항상 "static, 0 capture"로 정의되어 의미가 단일함.

## 모드별 가용성

- **POSIX-strict**: `function` 키워드 자체 금지. `f() { }` form만 가용, scope 없음 (전부 global).
- **POSIX-aware**: `function f { }`, `function f() { }`, capture list 모두 가용.
- **ksh93u-strict**: `function f { }`, `function f() { }` 가용. capture list는 ksh93에 없으므로 **parse error**.
- **ksh93u-aware**: 모두 가용 (기능 ceiling 없음).
- **default**: 모두 가용.

## 미결

남은 항목:
- Mutation 거부 시 정확한 error 메시지/exit code (impl detail)

기타 모두 project_kash_sweep_v1.md에서 해소.

**How to apply:** 함수 관련 후속 설계 (locals, nameref, namespace, discipline function 등)는 이 스코프 모델을 전제로 진행. zsh 기능 포팅 시 zsh의 모든 local 동작이 dynamic임을 감안해 어떻게 mapping할지 함께 검토.
