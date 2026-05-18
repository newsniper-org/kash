---
name: New shell — typeset attributes and user-defined types (committed)
description: typeset attribute set, typeset -T user-defined types, OOP-style 확장 (constructor/inheritance/private/static), introspection
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 typeset/타입 시스템 확정 사항. (관련: project_shell_compound_vars.md, project_shell_function_scope.md, project_shell_expansion_flags.md)

## Attribute 전체 카탈로그

ksh93의 모든 attribute 채택 + zsh의 유용한 일부:

| Flag | 의미 | 출처 |
|---|---|---|
| `-a` | indexed array | ksh/bash/zsh |
| `-A` | associative array | ksh/bash/zsh |
| `-C` | compound variable | ksh93 |
| `-T name=(...)` | **사용자 정의 type** | ksh93 |
| `-n` | nameref | ksh/bash/zsh |
| `-i [base]` | integer (optional base) | ksh/zsh |
| `-E [n]` | float exponential, n digit prec | ksh/zsh |
| `-F [n]` | float fixed, n digit prec | ksh/zsh |
| `-l` / `-u` | lowercase / uppercase on assign | ksh/bash/zsh |
| `-L n` / `-R n` | left/right justify, width n | ksh93 |
| `-Z n` | right-justify zero-pad (숫자) | ksh93 |
| `-r` | readonly | 공통 |
| `-x` | export | 공통 |
| `-M maptype` | map (read/write transformation) | ksh93 |
| `-H` | hostname (`/` ↔ `\` 변환) | ksh93 |
| `-t` | tag (user marker) | ksh93 |
| `-U` | unique array elements | zsh |

## Flag letter 충돌 해소 (ksh93 우선)

| Flag | 채택 의미 | 충돌 처리 |
|---|---|---|
| `-T` | type definition (ksh93) | zsh의 tied var는 `--tied=NAME,SEP` long form |
| `-H` | hostname (ksh93) | zsh의 hide value는 `--hide-value` long form (또는 보류) |
| `-t` | tag (ksh93) | bash의 trace function은 별도 letter 또는 미제공 |
| `-h` | zsh의 hide-from-outer (보류) | 새 셸 함수 스코프가 이미 static이라 의미 약함 |

## `typeset -T` 사용자 정의 type 코어 (ksh93 그대로)

```sh
typeset -T Stack=(
    typeset -a items
    typeset -i size=0
    
    function push { _.items[_.size]=$1; ((_.size++)); }
    function pop  { ((_.size--)); print ${_.items[_.size]}; unset _.items[_.size]; }
    function isEmpty { (( _.size == 0 )); }
)

Stack s              # type 이름이 declarator로 사용 (Stack s == typeset Stack s)
s.push apple
s.pop
Stack t=(items=(a b c) size=3)   # compound 리터럴로 초기화
```

**핵심 요소:**
- **`_`** = 메소드 내 현재 인스턴스 reference (ksh93 컨벤션). `$_`(이전 명령 인자)와 어휘 충돌이지만 `_.x` 형태는 항상 compound member access 컨텍스트라 모호성 없음.
- **멤버 declaration** — type body 내 `typeset` 호출 = 인스턴스 멤버 정의. 모든 attribute 가능 (int/float/array/assoc/compound/또 다른 type).
- **메소드 declaration** — `function name { }` — type에 lexically 바인딩, 인스턴스 method로 호출.
- **Type을 declarator로 사용** — `MyType var` ≡ `typeset MyType var`.
- **인스턴스 초기화** — compound 리터럴.
- **함수 스코프 정합** — 메소드는 기본 static, capture 가능 `function name(captured) { }`.

## OOP-style 확장 — 3종 확정 (세부는 project_kash_oop_extensions.md)

1. **Dunder methods**: lifecycle 2종만 — `__init` (constructor), `__del` (destructor). `__str` 등 다른 capability는 모두 typeclass로 (Showable 등).
2. **Private 멤버**: `private function` / `private typeset` 키워드, class-private (type 인스턴스 메소드만 접근).
3. **Static / class 멤버**: `static function` / `static typeset`, `TypeName.member` 접근, static 메소드 안에서는 `_` 미정의.

생성 form 3가지: `MyType x` / `MyType x(args)` / `MyType x=(field=val)`.

세부 의미론과 예시는 별도 메모리 (project_kash_oop_extensions.md).

**Inheritance 폐기 이유:** typeclass (Scala 3 모티브)가 inheritance의 모든 use case (행위 공유, 다형성, 코드 재사용, 인터페이스 강제)를 대체 가능. 두 메커니즘 공존은 학습 부담만 증가. Rust/Haskell 노선 채택.

## Typeclass 도입

Scala 3.x의 `given/using` 모델 기반 typeclass 채택. 자세한 설계는 별도 메모리 (project_shell_typeclass.md).

`typeset -T`와의 관계:
- `typeset -T`: 데이터 type (struct + 메소드)
- `typeclass`: 행위 contract (interface) + ad-hoc polymorphism
- 둘 다 공존, 데이터 모델링과 다형성의 분리

함수 type annotation 대체: type inference (runtime dispatch) + assertion (`[[ x -is T ]]`, `[[ x -satisfies Tc ]]`, `assert` builtin).

## Type introspection

- `${(t)var}` — type 이름 반환 (zsh expansion flag 확장)
  ```sh
  Stack s
  echo ${(t)s}             # "Stack"
  typeset -r Stack t
  echo ${(t)t}             # "Stack-readonly"
  ```
- `typeset -p var` — declaration 형태 출력 (재실행 가능 representation)

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `typeset` 자체 | × | ✓ | ✓ | ✓ | ✓ |
| `-a`/`-A`/`-i`/`-r`/`-x` (POSIX 인접) | × (`readonly`/`export`만) | ✓ | ✓ | ✓ | ✓ |
| `-C` compound | × | ✓ | ✓ | ✓ | ✓ |
| `-T` type definition | × | ✓ | ✓ | ✓ | ✓ |
| `-E`/`-F` floats | × | ✓ | ✓ | ✓ | ✓ |
| `-L`/`-R`/`-Z` justify | × | ✓ | ✓ | ✓ | ✓ |
| `-M` map | × | ✓ | ✓ | ✓ | ✓ |
| `-U` (zsh 채택) | × | ✓ | × | ✓ | ✓ |
| Dunder methods | × | ✓ | × | ✓ | ✓ |
| Private, static (확장 항목) | × | ✓ | × | ✓ | ✓ |
| Typeclass | × | ✓ | × | ✓ | ✓ |

## 미결 (후속 설계 대상)

(All initial 미결 resolved via OOP extensions and sweep_v1. Type composition (`typeset -T C=A+B`) 및 generic types는 v2+로 이관.)

**How to apply:** typeset 후속 확장 설계 시 (특히 위 4개 OOP 확장과 typeclass) 이 메모리의 모드별 가용성 표 형식 유지. capture/스코프 결정은 function_scope 메모리와 일관.
