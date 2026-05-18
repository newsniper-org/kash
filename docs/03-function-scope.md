# Function Scope and Capture

## 함수 정의 form별 의미

| Form | 스코프 | Capture |
|---|---|---|
| `f() { }` | dynamic (POSIX form, caller scope 공유) | 해당 없음 |
| `function f { }` | static (caller의 local/typeset 안 보임) | 없음 |
| `function f() { }` | static (위와 동의어, 빈 capture list로 처리) | 없음 |
| `function f(a, b, ...) { }` | static + 명시된 이름만 caller에서 import | read-only by-ref |

`function f() { }`는 ksh93의 footgun (bash와 다른 의미)을 제거 — 항상 "static, 0 capture"로 단일 정의.

## Capture semantics

### By reference, read-only

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

### Caller scope = dynamic call chain 전체 traverse

- 직속 caller에 없으면 chain 위로 올라가며 찾음.
- 어디에도 없으면 unset (`$undeclared`와 동일 처리).
- strict 모드에서 unset → error 격상 여부 미결.

### Attributes 전파

`typeset -i`, `-a`, `-A` 등 attributes는 reference를 통해 자동으로 전파.

## 설계 정당성

- Static base + opt-in dynamic capture는 leaky locals 함정과 정보 전달 verbose함을 *둘 다* 회피.
- Read-only capture 덕분에 함수 시그니처 = "외부에서 읽는 변수의 완전한 명세."
- Read vs write 권한이 문법적으로 분리 — capture는 read 전용, write는 nameref나 unguarded global을 통해서만 가능 (둘 다 명시적이라 reviewer가 식별 가능).
- `function f() { }`의 ksh93 footgun 제거.

## 모드별 가용성

- **POSIX-strict**: `function` 키워드 자체 금지. `f() { }` form만 가용, scope 없음 (전부 global).
- **POSIX-aware**: `function f { }`, `function f() { }`, capture list 모두 가용.
- **ksh93u-strict**: `function f { }`, `function f() { }` 가용. capture list는 ksh93에 없으므로 **parse error**.
- **ksh93u-aware**: 모두 가용 (기능 ceiling 없음).
- **default**: 모두 가용.

## 미결

- Mutation 거부 시 정확한 에러 메시지/exit code
- nameref ↔ captured 변수의 transitive read-only edge case
- positional parameter (`$1` 등) capture 가능성 (현재는 named var만)
- Capture list trailing comma 허용 여부
- Strict 모드에서 unfound capture가 error로 격상되는지
