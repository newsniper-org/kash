# Compound Variables and Member Access

## Type 구분 (Compound vs Associative)

별개 type 유지.

- **Compound (`typeset -C`)**: 멤버 이름이 식별자(변수명 규칙). schema-like struct. 각 멤버 type 별도 가능 (scalar/compound/indexed/assoc/function).
- **Associative (`typeset -A`)**: 키가 임의 문자열. 값은 보통 동질 (scalar). 명시적 map.

의미적 차이: Compound = struct, Assoc = map. 코드에서 의도가 드러나야 함.

## Member access — Option C + strict typing

- **Compound 멤버 접근**: `${person.name}` 과 `${person[name]}` **둘 다 가용 (동의어)**. ksh93 정합.
- **Assoc 키 접근**: `${assoc[key]}` **만**. `.` 접근 거부.
- **Cross-access는 strict 모드 error**:
  - assoc에 `.` 접근 (`${assoc.foo}`) → default 모드에서 error (ksh93u-strict는 호환을 위해 허용)
  - compound에 임의 문자열 키 접근 → strict 모드 error

시각적 규칙: `.` 보이면 무조건 compound, `[]` 보이면 declared type에 따라 결정.

## Compound 선언/리터럴

- **명시 선언**: `typeset -C var`
- **자동 추론**: `var=(...)` 형태의 compound 리터럴 할당 시 자동으로 compound가 됨.
- **빈 compound**: `typeset -C empty`
- **리터럴 문법**: JSON-like nested
  ```sh
  config=(host="x.com" port=80 tls=(cert="..." key="...") tags=(a b c))
  ```

## `.` 어휘 충돌 처리

- **`$foo.bar` (no braces)**: `${foo}.bar`로 파싱. 멤버 접근은 `${foo.bar}` 필수. 배열 `${arr[i]}` 강제 규칙과 일관.
- **`.` builtin (POSIX source)**: 공백 분리 필수 — `. script` ✓, `.script` ✗.
- **변수명에 `.` 포함**: compound member의 일부로서만 합법.

## Discipline functions — 완전 보존

- `function var.member.set { ... }` — 할당 시 호출
- `function var.member.get { ... }` — 읽기 시 호출
- `function var.member.unset { ... }` — `unset` 시 호출
- `function var.member.append { ... }` — `+=` 시 호출
- 컨텍스트 변수 `.sh.value`, `.sh.subscript` 보존.

## Nameref

- 깊은 멤버 alias: `typeset -n ref=a.b.c.d` 가능
- Capture된 read-only 변수의 nameref는 read-only 전염

## `.sh.*` 네임스페이스

- ksh93 호환을 위해 보존 (`.sh.version`, `.sh.match`, `.sh.subshell`, `.sh.value` 등)
- 신규 쉘 고유 특수 변수는 별도 prefix (쉘 이름 정해지면 `.<name>.*`)

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

- 멤버 type 변환 — `person.x=5` (scalar)였다가 `person.x=(...)` (compound) 재할당 시 동작
- `typeset -n ref=person` 후 `ref.x=...` 시 type 호환성
- compound 멤버 iteration 문법 — `${!person.@}` 보존 + zsh `${(k)compound}` 통일
- compound vs assoc의 동적 type 변경 → 권장 비허용
