# Array and Associative Array

## 확정 사항

- **Indexing base: 0-based** — ksh93/bash 호환 우선. ($1 positional vs arr[0]의 내부 불일치는 받아들임)
- **Sparse 허용** — dense는 sparse의 특수 케이스. ksh/bash 정합.
- **Indexed 리터럴**: `arr=(a b c)`
- **Associative 리터럴**: `arr=([k1]=v1 [k2]=v2)` (ksh/bash form). zsh의 flat pair (`arr=(k v k v)`) 거부 — silent corruption 위험.
- **Negative indexing**: `${arr[-1]}` = 마지막 원소.
- **Bare `$arr`** = `${arr[0]}` (ksh/bash semantics; first element only, no auto-expansion).
- **Bare `$arr[i]` (braces 없이)**: parse error. `${arr[i]}` 강제. POSIX parameter expansion 문법 일관성.
- **Nested compound variables**: ksh93의 `arr=(a=(...) b=(...))` 보존 + 확장 (이 쉘의 고유 강점).

## Slicing 문법 (모드별)

| 모드 | Slicing form |
|---|---|
| POSIX-strict | 배열 자체 비활성화 |
| POSIX-aware | ksh form `${arr[@]:s:n}` (start+count)만 |
| ksh93u-strict | ksh form만 |
| default | ksh form + zsh form (start+end, 새 표기로 재설계 필요) 둘 다 |

zsh form의 정확한 새 표기는 미결 — ksh `[s:n]`과 직교한 form 필요 (`${arr[@]:s..e}` 같은).

## Implicit creation 모드별 차등

`arr[5]=x` 미선언 상태에서:

| 모드 | 동작 |
|---|---|
| default, POSIX-aware | 허용 |
| ksh93u-strict | 허용 (ksh93 호환) |
| POSIX-strict | 배열 자체 없음 |

## Indexed array에 string key 인덱싱 (`arr[hello]`)

| 모드 | 동작 |
|---|---|
| POSIX-aware | 산술 평가 → 0 취급 (ksh/bash 호환, footgun) |
| ksh93u-strict, default | **strict mode error** |

## 모드별 가용성 요약

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| 배열 자체 | × | ✓ | ✓ | ✓ | ✓ |
| `arr=(a b c)`, `${arr[i]}` 등 ksh form | × | ✓ | ✓ | ✓ | ✓ |
| Nested compound | × | ✓ | ✓ | ✓ | ✓ |
| Negative indexing | × | ✓ | × (ksh93에 없음) | ✓ | ✓ |
| zsh-style slicing | × | × | × | ✓ | ✓ |
| Implicit creation | × | ✓ | ✓ | ✓ | ✓ |
| Indexed에 string key | × | ✓ (산술 평가 0) | × | ✓ (산술 평가 0) | × (error) |

## 미결

- zsh form slicing의 정확한 새 표기
- `typeset -i arr` 같은 attribute가 원소 전체에 일관 적용 (당연하지만 명시 필요)
- Empty array 검사 canonical form (`${#arr[@]} == 0`로 충분할 것)
