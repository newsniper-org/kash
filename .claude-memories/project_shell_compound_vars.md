---
name: New shell — compound variables and member access (committed)
description: Compound variable design — member access via `.` and `[]`, compound vs assoc 별개 type, discipline functions 보존, strict typing
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 compound variable / associative array 시스템 확정 사항. (관련: project_shell_arrays.md)

## 확정 사항

### Type 구분
- **Compound (`typeset -C`)** 와 **Associative (`typeset -A`)** 는 **별개 type 유지**.
  - Compound: 멤버 이름이 식별자(변수명 규칙). schema-like struct. 각 멤버 type 별도 가능 (scalar/compound/indexed/assoc/function).
  - Associative: 키가 임의 문자열. 값은 보통 동질 (scalar). 명시적 map.
- 의미적 차이: Compound = struct, Assoc = map. 코드에서 의도가 드러나야 함.

### Member access — Option C + strict typing
- **Compound 멤버 접근**: `${person.name}` 과 `${person[name]}` **둘 다 가용 (동의어)**. ksh93 정합.
- **Assoc 키 접근**: `${assoc[key]}` **만**. `.` 접근 거부.
- **Cross-access는 strict 모드 error**:
  - assoc에 `.` 접근 (`${assoc.foo}`) → default 모드에서 error (ksh93u-strict는 ksh93 호환을 위해 허용)
  - compound에 임의 문자열 키 접근 (식별자 아닌 키) → strict 모드 error
- 시각적 규칙: `.` 보이면 무조건 compound, `[]` 보이면 declared type에 따라 결정.

### Compound 선언/리터럴
- **명시 선언**: `typeset -C var`
- **자동 추론**: `var=(...)` 형태의 compound 리터럴 할당 시 자동으로 compound가 됨.
- **빈 compound**: `typeset -C empty` (멤버 0개 상태 합법)
- **리터럴 문법**: JSON-like nested (ksh93 그대로)
  ```sh
  config=(host="x.com" port=80 tls=(cert="..." key="...") tags=(a b c))
  ```

### `.` 어휘 충돌 처리
- **`$foo.bar` (no braces)**: `${foo}.bar` 로 파싱. 멤버 접근은 `${foo.bar}` 필수. 배열 `${arr[i]}` 강제 규칙과 일관.
- **`.` builtin (POSIX source)**: 공백 분리 필수 — `. script` ✓, `.script` ✗. ksh93 그대로.
- **변수명에 `.` 포함**: compound member의 일부로서만 합법.

### Discipline functions — 완전 보존
- `function var.member.set { ... }` — 할당 시 호출
- `function var.member.get { ... }` — 읽기 시 호출
- `function var.member.unset { ... }` — `unset` 시 호출
- `function var.member.append { ... }` — `+=` 시 호출
- 컨텍스트 변수 `.sh.value`, `.sh.subscript` 보존.

### Nameref
- 깊은 멤버 alias: `typeset -n ref=a.b.c.d` 가능
- Capture된 read-only 변수의 nameref는 read-only 전염 (function scope 결정과 정합)

### `.sh.*` 네임스페이스
- ksh93 호환을 위해 **보존** (`.sh.version`, `.sh.match`, `.sh.subshell`, `.sh.value` 등 모두 그대로)
- 신규 쉘 고유 특수 변수는 **별도 prefix** 사용 검토 (쉘 이름 정해지면 `.<name>.*` 형태)

### Indexed array에 string key 인덱싱
- 배열 결정에서 미결로 남겼던 항목 해소: **strict 모드 (POSIX-strict, ksh93u-strict, default)에서 error로 격상.**
- POSIX-aware에서는 ksh/bash 동작 보존 (산술 평가 → 0 취급, footgun이지만 호환).

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| Compound var | × | ✓ | ✓ | ✓ | ✓ |
| `.` member access | × | ✓ | ✓ | ✓ | ✓ |
| Compound `[]` 접근 | × | ✓ | ✓ | ✓ | ✓ |
| Assoc에 `.` 접근 | × | × | ✓ (ksh93 호환) | ✓ (ksh93 lenient) | × (strict typing) |
| Compound에 임의 문자열 키 | × | × | ✓ | ✓ | × |
| Discipline functions | × | ✓ | ✓ | ✓ | ✓ |
| `.sh.*` 보존 | × | ✓ | ✓ | ✓ | ✓ |
| Indexed에 string key | × | ✓ (산술 평가) | × | ✓ (산술 평가) | × |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** zsh parameter expansion flags `${(flags)var}` 설계 시, `(...)` 가 `${` 직후에 오면 flag로 파싱한다는 규칙을 확정하면 compound `${var.member}` 와 직교. compound member에 flag 적용 (`${(P)var.member}` 등) 의미는 expansion flags 설계 단계에서 함께 결정.
