# Standard Typeclass Library

## Namespace 및 import

- 위치: `.kash.std`
- **Auto-import in default / POSIX-aware / ksh93u-aware** (prelude)
- POSIX-strict / ksh93u-strict 비활성 (typeclass 자체 비활성)

## v1 prelude — 6개 typeclass

### `Eq`
```kash
typeclass .kash.std.Eq { function eq; }
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
typeclass .kash.std.Showable { function show; }
```
`print "$x"`에서 compound/user type 인스턴스면 자동 `show` 호출.

### `Hashable requires Eq`
```kash
typeclass .kash.std.Hashable requires .kash.std.Eq { function hash; }
```
assoc array 키 사용 필수. Hashable 없으면 키 사용 시 error.

### `Iterable`
```kash
typeclass .kash.std.Iterable { function iter; }
```
`iter`는 generator — `yield`로 element 반환.

### `Callable`
```kash
typeclass .kash.std.Callable { function call; }
```
**Parser sugar 없음** — 명시적 `inst.call args` 만. (sugar는 v2+ 검토)

## `yield` 키워드 (신규)

- generator 함수 내부 — 값 yield + 함수 일시 정지, caller 요청 시 resume
- async/await와 별개 (iteration 전용, Python generator 패턴)

```kash
function range {
    local i=0 n=$1
    while ((i < n)); do
        yield $i
        ((i++))
    done
}

# Eager
for x in $(range 5); do echo $x; done

# Lazy (for-loop + Iterable 통합)
instance .kash.std.Iterable for SomeType { function iter { range $_.count; } }
for x in someinst; do echo $x; done    # lazy iter 호출
```

`for x in EXPR`:
- EXPR 단일 변수 + Iterable instance 보유 → `EXPR.iter` lazy iterate
- 그 외 → POSIX (IFS word-split)

## Built-in type default instance

### Numeric (int*/uint*/float*/bfloat16/complex*/bcomplex32)
- `Eq`, `Showable`, `Hashable` 자동
- `Ord` 자동 — **complex/bcomplex 제외** (IEEE 754 no total order)

### 기타
- `string`: `Eq`, `Ord` (lex), `Showable`, `Hashable`
- `bool`: 전부
- indexed array: `Iterable` (element)
- assoc array: `Iterable` (keys)
- compound var: 자동 instance 없음 — user 정의 필요

## v1 안 포함 (v2+)

`Cloneable`, `Reprable`, `Num`/`Add`/`Mul`, `From`/`Into`, `Default`, `Collection`.

## Shadowing

사용자 정의 typeclass와 prelude 충돌 시:
- Lexical wins (사용자 우선)
- Prelude 명시 접근: `.kash.std.Eq`

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `.kash.std.*` prelude | × | ✓ (auto) | × | ✓ | ✓ |
| 6 typeclasses | × | ✓ | × | ✓ | ✓ |
| Built-in default instances | × | ✓ | × | ✓ | ✓ |
| `yield` | × | ✓ | × | ✓ | ✓ |
| Iterable for-loop 통합 | × | ✓ | × | ✓ | ✓ |

## 미결

- `yield` 인자 없음 (빈 string vs error)
- Generator early-finalization
- Generator state/stack 저장 구현
- Iterable for-loop의 lazy vs eager 분기 정확한 규칙
- complex partial order (별도 typeclass v2+)
- Compound var Showable fallback 형식
