---
name: New shell — emulation modes design (committed)
description: 신규 쉘의 mode/emulation 시스템 — 어떤 모드를 둘지, 스코프 어떻게 되는지 등의 확정 사항
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 설계에서 *확정된* 모드 시스템 결정 사항. (관련: project_shell_design.md)

## 모드 종류 (5개)
- **POSIX-strict**: POSIX에 정의된 기능만 사용 가능. `[[ ]]`, `((...))`, `function` 키워드, 배열, compound var 등 비-POSIX 확장은 parse/eval 단계에서 거부.
- **POSIX-aware**: 모든 확장 사용 가능. corner case 의미론(단어 분할, `echo` escape, `kill -l` 출력 형식 등)에서 POSIX 명세를 따름.
- **ksh93u-strict**: ksh93u+m 매뉴얼에 문서화된 기능만 사용 가능. 새 쉘이 추가한 확장(zsh 포팅분 등)은 비활성화.
- **ksh93u-aware**: 모든 기능 가용 (default와 동일 set). 단, ksh93과 default가 충돌하는 corner case에서 ksh93 쪽 의미론 채택. 구체:
  - Null glob 기본: unchanged (POSIX 기본, ksh93 정합) — `-secure` 결합 시에만 fail
  - Assoc에 `.` 접근: 허용 (ksh93 lenient)
  - Compound에 임의 문자열 키 접근: 허용 (ksh93 lenient)
  - Indexed array에 string key 인덱싱: silent eval to 0 (ksh93/bash 호환)
  - 중복 기능(예: `${!person.@}` vs `${(k)person}`) 권장 form: ksh form 우선
- **default**: 전체 기능 세트 가용. 충돌점에서 새 쉘의 더 안전한 default 선택 (strict typing, fail-glob 등).

## strict vs aware의 본질
- **strict** = 기능 가용성 ceiling (확장 disable, parse/eval reject)
- **aware** = 런타임 의미론 dial preset (충돌 시 어느 쪽 규칙 따를지)
- 둘은 다른 차원이므로 한 모드 안에서 동시에 다룰 수 있음 (예: 향후 "POSIX-strict + ksh93u-aware" 같은 조합도 이론상 가능).

## 스코핑
**Lexical scoping 채택.** 동적 상속이 아님.
- `posix-strict` 블록 안에서 호출된 함수는, 그 함수가 *정의된 시점의 모드*를 따름.
- zsh의 `emulate -L` 의미론과 동일.

## 스코프 단위 (3단계 모두 채택)
1. **파일 수준**: `pragma mode <name>` (또는 shebang `--mode=<name>`)
2. **함수 수준**: `emulate -L <name>` 식 — 함수 종료 시 자동 복원
3. **블록 수준**: `mode <name> { ... }` — 새 키워드 `mode` 도입 필요 (ksh93에는 없음)

## Mode modifiers (suffix)

기본 모드 4종에 *modifier suffix*를 붙여서 추가 제약을 거는 architecture 채택. 첫 사례:

- **`-secure`**: scope 내에서 unsafe/footgun 기능 차단. *"보안"이 아니라 footgun-elimination 프로파일*. 현재 적용 항목 (project_shell_set_options.md에서 완전 정리):
  - `errexit`, `pipefail`, `nounset`, `noclobber` 강제 on (lock)
  - Null glob → fail 강제 (lock)
  - `(e)` re-evaluation 금지 (lock)
  - Backticks 금지 (lock)
  - `warn-backticks`, `warn-unsafe-eval`, `warn-implicit-array`, `warn-leaky-glob` 강제 on (lock)
  - `error-leaky-jobs` 강제 on (lock), 다른 두 leaky-jobs 옵션 (`warn-`/`ask-`)은 off lock — MX 보장
  - `eval` builtin 차단 (Shellshock 정책 P3 — project_kash_security_policy.md)
  - 모두 modifier monotonicity 적용 — scope 내에서 끄려는 시도는 error
  - 향후 후보: `eval` builtin, dynamic path `source`, `printf -v` to dynamic name, `-no-network` 같은 별도 modifier 등.

조합 표기: `default-secure`, `POSIX-aware-secure`, `ksh93u-strict-secure` 등. 즉 mode space가 `<base>-<modifier>` 형태로 확장 가능.

향후 추가될 가능성 있는 modifier 예 (미결): `-noglob`, `-noeval`, `-readonly-env` 등. 모두 base mode에 *제약 추가* 방향으로만 동작 (확장이 아니라).

## 미결 사항

남은 항목 (open-ended / impl detail):
- 각 strict 모드 disable 기능의 *완전한* 카탈로그 — 누적 진행 중 (각 메모리에 분산 명시됨, 향후 통합 spec 문서화)
- POSIX-aware의 corner case 카탈로그 — 누적 진행 중

기타 모두 project_kash_sweep_v1.md / project_shell_mode_syntax.md에서 해소 — `mode` 키워드, 여러 modifier 조합 허용, modifier inner 제거 불가, 모드 조합은 한 블록에 하나 (base + modifiers).

**How to apply:** 향후 설계 결정 시 이 모드 시스템에 영향이 가는 사항(예: 새 keyword 추가, parse-time 의미 변경)은 위 4개 모드 각각에서 어떻게 동작할지 함께 결정할 것. "default 모드에서만" 같은 결정도 명시적으로 기록.
