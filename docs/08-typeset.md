# Typeset Attributes and User-Defined Types

## Attribute 전체 카탈로그

ksh93의 모든 attribute + zsh의 유용한 일부.

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
| `-H` | hostname (ksh93) | zsh의 hide value는 `--hide-value` long form |
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

Stack s              # type 이름이 declarator (Stack s == typeset Stack s)
s.push apple
s.pop
Stack t=(items=(a b c) size=3)   # compound 리터럴로 초기화
```

### 핵심 요소

- **`_`** = 메소드 내 현재 인스턴스 reference (ksh93 컨벤션). `$_` (이전 명령 인자)와 어휘 충돌이지만 `_.x` 형태는 항상 compound member access 컨텍스트라 모호성 없음.
- **멤버 declaration** — type body 내 `typeset` 호출 = 인스턴스 멤버 정의. 모든 attribute 가능.
- **메소드 declaration** — `function name { }` — type에 lexically 바인딩.
- **Type을 declarator로 사용** — `MyType var` ≡ `typeset MyType var`.
- **인스턴스 초기화** — compound 리터럴.
- **함수 스코프 정합** — 메소드는 기본 static, capture 가능 `function name(captured) { }`.

## OOP-style 확장 — 3종 도입 확정, 세부 미설계

원래 4종이었으나 typeclass 도입으로 "inheritance" 폐기.

1. **Constructor/destructor (dunder methods)** — `__init`, `__del`, `__str` 등 Python-style dunder. 호출 시점 등 세부 후속.
2. **Private 멤버** — 컨벤션 (`_prefix`)만인지 언어 차원 access modifier인지 후속.
3. **Static / class 멤버** — type 자체에 묶이는 멤버. 후속.

**Inheritance 폐기 이유:** typeclass (Scala 3 모티브)가 inheritance의 모든 use case (행위 공유, 다형성, 코드 재사용, 인터페이스 강제)를 대체. 학습 부담 감소. Rust/Haskell 노선.

## Typeclass 도입

별도 문서: [Typeclasses](09-typeclass.md).

`typeset -T`와의 관계:
- `typeset -T`: 데이터 type (struct + 메소드)
- `typeclass`: 행위 contract (interface) + ad-hoc polymorphism

## Type introspection

- `${(t)var}` — type 이름 반환 (zsh expansion flag 확장)
  ```sh
  Stack s
  echo ${(t)s}             # "Stack"
  typeset -r Stack t
  echo ${(t)t}             # "Stack-readonly"
  ```
- `typeset -p var` — declaration 형태 출력

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

## 미결

- Dunder 메소드 정확한 목록
- Constructor 호출 시점: `MyType x` 만으로? `MyType x()`? `MyType x(arg1, arg2)`?
- Private/protected access modifier 깊이 (컨벤션 vs 언어 차원)
- Static 멤버 문법 (`static function`, `static typeset` 등)
- Type composition 가능성 (`typeset -T C=A+B`) — typeclass와 별개의 데이터 합성
- Generic types 도입 여부 (v1 보류 권장)
