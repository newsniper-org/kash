---
name: kash — standard typeclass library (committed)
description: .kash.std prelude — Eq/Ord/Showable/Hashable/Iterable/Callable 6종, built-in type 자동 instance, yield 키워드 도입
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 표준 typeclass library (prelude). (관련: project_shell_typeclass.md, project_kash_oop_extensions.md, project_shell_arithmetic.md)

## Namespace 및 import 정책

- 위치: **`.kash.std`** namespace
- **Auto-import in default / POSIX-aware / ksh93u-aware modes** (prelude — implicit `use namespace .kash.std`)
- POSIX-strict / ksh93u-strict: 비활성 (typeclass 자체 비활성)

## v1 prelude — 6개 typeclass

### `Eq`
```kash
typeclass .kash.std.Eq {
    function eq          # _.eq other → truthy/falsy
}
```

### `Ord requires Eq`
```kash
typeclass .kash.std.Ord requires .kash.std.Eq {
    function lt
    function le { _.eq $1 || _.lt $1; }
    function gt { ! _.le $1; }
    function ge { ! _.lt $1; }
}
```

### `Showable`
```kash
typeclass .kash.std.Showable {
    function show        # _.show → string
}
```
dunder `__str` 대체. `print "$x"`에서 compound/user type 인스턴스면 자동 `show` 호출.

### `Hashable requires Eq`
```kash
typeclass .kash.std.Hashable requires .kash.std.Eq {
    function hash        # _.hash → integer
}
```
assoc array 키 사용 필수. Hashable 없는 type을 assoc 키로 쓰면 error.

### `Iterable`
```kash
typeclass .kash.std.Iterable {
    function iter        # _.iter는 generator — yield로 element 반환
}
```

### `Callable`
```kash
typeclass .kash.std.Callable {
    function call        # _.call args... → result
}
```
**Parser sugar 없음** — `c args` 같은 묵시적 호출 불가. 사용자 명시 `c.call args`. 모호성 zero.

## `yield` 키워드 (신규)

- generator 함수 내부에서 사용 — caller에게 값 건네고 함수 일시 정지
- caller가 다음 값 요청 시 resume
- async/await와는 *별개* — iteration 전용 (Python generator 패턴)

```kash
function range {
    local i=0 n=$1
    while ((i < n)); do
        yield $i
        ((i++))
    done
}

# Eager (command sub) — 모든 yield 수집, newline 구분
for x in $(range 5); do echo $x; done

# Lazy (for-loop + Iterable parser 통합)
instance .kash.std.Iterable for SomeType {
    function iter { range $_.count; }
}
for x in someinst; do echo $x; done    # iter 호출, lazy
```

### `yield` 의미론
- generator 함수 외부에서 사용 → parse/runtime error
- 인자 없으면 빈 string yield (vs error — 미결)
- generator 종료 → caller iterator `done`
- Early break → cleanup (finally 등)

### `for x in EXPR` parser 동작 (수정 — Iterable 통합)
- EXPR가 단일 변수 AND 그 값의 type이 `Iterable` instance 보유 → `EXPR.iter` 호출하여 lazy iterate
- 그 외 → POSIX 의미 (IFS word-split)

## Built-in type default instance

### 모든 primitive numeric type
(`int8-128`, `uint8-128`, `float16-128`, `bfloat16`, `complex32-256`, `bcomplex32`)
- `Eq`, `Showable`, `Hashable` 자동
- `Ord` 자동 — **단 complex/bcomplex 제외** (IEEE 754 complex no total order)

### 기타 built-in
- `string`: `Eq`, `Ord` (lex), `Showable` (identity), `Hashable`
- `bool`: 전부
- indexed array of T: `Iterable` 자동 (element iterate)
- assoc array of T: `Iterable` 자동 (keys iterate)
- compound var: 자동 instance 없음 — user 정의 type이 explicit 제공해야

## v1 prelude에 안 포함 (v2+ 검토)

| Typeclass | 이유 |
|---|---|
| `Cloneable` | 셸 copy-by-value 기본 가정. 깊은 복사는 v2 |
| `Reprable` | Showable과 너무 비슷 — debug 차별화 v2 |
| `Num`/`Add`/`Mul` 등 | user 타입 산술 operator overload — strong opinion 필요. v2 |
| `From`/`Into` | type-name function form (`int8(val)`)로 충분. v2 typeclass화 |
| `Default` | v2 |
| `Collection`/`Container` | first-class 자료구조 별도 결정 후 |

## Method 충돌 / Shadowing

사용자가 prelude typeclass와 같은 이름 (`Eq` 등) 으로 자기 typeclass 정의 시:
- Lexical scope에서 사용자 정의 우선 (lexical wins)
- Prelude 명시 접근은 항상 full path: `.kash.std.Eq`

## `Showable` 자동 호출 메커니즘

`print "$x"`, `printf "%s" "$x"` 등에서:
- `x`가 compound var 또는 user-defined type 인스턴스
- → `x.show` 호출 결과를 문자열로 사용 (Showable instance 있으면)
- 없으면 type/structure dump (fallback)

이게 dunder `__str` 대신 typeclass인 이유 — 외부 instance 추가 가능 (retroactive).

## Hashable + assoc array

```kash
typeset -T Point=(typeset -i x y)
instance .kash.std.Hashable for Point { function hash { print $((_.x * 31 + _.y)); } }
instance .kash.std.Eq for Point { function eq { (( _.x == $1.x && _.y == $1.y )); } }

typeset -A points
Point p=(x=3 y=4)
points[$p]="origin region"
```

Hashable 없는 type → assoc 키 사용 시 error.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `.kash.std.*` prelude | × | ✓ (auto) | × | ✓ | ✓ |
| `Eq/Ord/Showable/Hashable/Iterable/Callable` | × | ✓ | × | ✓ | ✓ |
| Built-in type default instances | × | ✓ | × | ✓ | ✓ |
| `yield` 키워드 | × | ✓ | × | ✓ | ✓ |
| `for x in iterable_var` lazy iteration | × | ✓ | × | ✓ | ✓ |

## 미결

남은 v2+/impl 항목:
- Generator state/stack 저장 구현 (Rust coroutine vs state machine vs OS thread) — impl
- Complex/bcomplex partial order 제공 (`PartialOrd` 별도 typeclass) — v2+

기타 모두 project_kash_sweep_v1.md에서 해소 — `yield` 인자 없음 error, early-finalization finally 실행 보장, for-loop parse-time check, Showable fallback JSON-like dump.

**How to apply:** 향후 standard typeclass 추가 시 `.kash.std.*` namespace 일관. yield 의미는 generator-only — 동시성/async와 분리 유지.
