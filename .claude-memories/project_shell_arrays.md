---
name: New shell — array and associative array semantics (committed)
description: 배열/연관배열 의미론 — 0-based, sparse, ksh-style 선언, 슬라이싱, 모드별 가용성 확정
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 배열 시스템 확정 사항. (관련: project_shell_design.md, project_shell_modes.md, project_shell_function_scope.md)

## 명시적으로 확정된 결정

- **Indexing base: 0-based** — ksh93/bash 호환 우선. ($1 positional vs arr[0]의 내부 불일치는 받아들임)
- **Bare `$arr`** = `${arr[0]}` — ksh/bash semantics. zsh의 auto-expansion 채택 안 함.
- **Slicing 문법**:
  - default mode: ksh form `${arr[@]:s:n}` (start+count) + zsh form (start+end, 새 표기로 재설계 필요) 둘 다.
  - POSIX-aware: ksh form만 (신규 슬라이싱 form은 새 쉘 확장).
  - ksh93u-strict: ksh form만.
  - POSIX-strict: 배열 자체 비활성화.
- **Implicit creation** (`arr[5]=x` 미선언 상태): 모드별 차등.
  - default, POSIX-aware: 허용
  - ksh93u-strict: 허용 (ksh93 호환)
  - POSIX-strict: 배열 자체 없음

## 사용자 silence를 수락으로 해석한 권장안 (재논의 필요 시 flag)

- **Sparse 허용**: dense는 sparse의 특수 케이스. ksh/bash 정합.
- **Associative 리터럴**: `arr=([k1]=v1 [k2]=v2)` (ksh/bash form). zsh의 flat pair (`arr=(k v k v)`) 거부 — silent corruption 위험.
- **Nested compound variables**: ksh93의 `arr=(a=(...) b=(...))` 보존 + 확장. **이 쉘의 고유 강점**. 구체 멤버 접근 문법은 compound var 설계에서.
- **Negative indexing**: `${arr[-1]}` = 마지막. bash 4.3+ / zsh와 정합. ksh93에 없지만 호환성 깨지 않음.
- **Bare `$arr[i]` (braces 없이)**: parse error. `${arr[i]}` 강제. POSIX parameter expansion 문법 일관성.

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

남은 impl detail:
- ksh93u-strict 모드 negative index 차단 정확한 error 메시지 (impl)
- `<N-M>` glob range leading zero 처리 자릿수 매칭 정책 (glob_pattern과 연관)

기타 모두 project_kash_sweep_v1.md / compound_vars 결정에서 해소 — slicing `${arr[@]:s..e}`, attribute 전파 일관, empty check `${#arr[@]} == 0`.

**How to apply:** 배열 관련 후속 결정 (compound var, expansion flags, iteration, pattern matching의 배열 인자 처리 등)은 위 사항을 전제로 진행. zsh 기능 포팅 시 zsh의 1-based 의미를 0-based로 명시적 mapping 필요.
