# Mode System

쉘은 여러 의미론 모드를 가진다 — 어떤 모드에 있느냐에 따라 기능 가용성과 의미론이 달라진다.

## Base modes (5종)

- **POSIX-strict**: POSIX에 정의된 기능만 사용 가능. `[[ ]]`, `((...))`, `function` 키워드, 배열, compound var 등 비-POSIX 확장은 parse/eval 단계에서 거부.
- **POSIX-aware**: 모든 확장 사용 가능. corner case 의미론(단어 분할, `echo` escape, `kill -l` 출력 형식 등)에서 POSIX 명세를 따름.
- **ksh93u-strict**: ksh93u+m 매뉴얼에 문서화된 기능만 사용 가능. 새 쉘이 추가한 확장(zsh 포팅분 등)은 비활성.
- **ksh93u-aware**: 모든 기능 가용 (default와 동일 set). 단, ksh93과 default가 충돌하는 corner case에서 ksh93 쪽 의미론 채택:
  - Null glob 기본: unchanged (POSIX/ksh93 정합) — `-secure` 결합 시에만 fail
  - Assoc에 `.` 접근: 허용 (ksh93 lenient)
  - Compound에 임의 문자열 키 접근: 허용 (ksh93 lenient)
  - Indexed array에 string key 인덱싱: silent eval to 0 (ksh93/bash 호환)
  - 중복 기능 권장 form: ksh form 우선 (예: `${!person.@}` over `${(k)person}`)
- **default**: 전체 기능 세트 가용. 충돌점에서 새 쉘의 더 안전한 default 선택 (strict typing, fail-glob 등).

## strict vs aware의 본질

- **strict** = 기능 가용성 ceiling (확장 disable, parse/eval reject)
- **aware** = 런타임 의미론 dial preset (충돌 시 어느 쪽 규칙 따를지)

## Mode modifiers (suffix)

기본 모드에 modifier suffix를 붙여서 추가 제약. 첫 사례:

- **`-secure`**: scope 내에서 unsafe/footgun 기능 차단. 단순 "보안"이 아니라 *footgun-elimination 프로파일*. 현재 적용 항목:
  - `(e)` re-evaluation (expansion flag) 금지
  - Null glob fail mode 강제
  - 향후 후보: `eval` builtin, dynamic path `source`, `printf -v` to dynamic name, `set -u` 강제, `set -o pipefail` 강제 등

조합 표기: `default-secure`, `POSIX-aware-secure`, `ksh93u-strict-secure` 등. 즉 mode space는 `<base>-<modifier>` 형태.

향후 추가 가능 modifier: `-noglob`, `-noeval`, `-readonly-env` 등.

## Scoping

**Lexical scoping 채택.** 동적 상속이 아님.
- 한 함수가 어느 모드에서 호출되든, 그 함수가 *정의된 시점의 모드*를 따름.
- zsh의 `emulate -L` 의미론과 동일.

## Scope 단위 (3단계)

1. **파일 수준**: `pragma mode <name>` (또는 shebang `--mode=<name>`)
2. **함수 수준**: `emulate -L <name>` 식 — 함수 종료 시 자동 복원
3. **블록 수준**: `mode <name> { ... }` — 새 키워드 `mode` 도입

구체 문법은 [Mode declaration syntax](02-mode-syntax.md) 참조.

## Modifier monotonicity

Inner scope에서 modifier 추가는 가능하나 제거는 불가. 안전 modifier가 silent하게 풀리는 것 방지.
