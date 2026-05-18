---
name: New shell (kash) — subshell, pipeline, coprocess, process substitution (committed)
description: subshell 의미론 (POSIX), `|&` coprocess (ksh93 baseline), 모드별 pipeline 마지막 cmd 처리, coproc + process subst 채택
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 kash의 실행 layer 첫 결정. (관련: project_shell_modes.md)

## 핵심 결정

### (A) `|&` = coprocess (모든 모드)
- ksh93 baseline 따름. bash/zsh의 stderr+stdout pipe 의미 *거부*.
- stderr+stdout pipe는 `cmd 2>&1 | cmd2` 명시적 표기.
- bash/zsh muscle memory 사용자에게 known incompatibility로 문서화.
- 이건 *구문 의미*라 모드 무관 동일 (`(B)`와 차이 — 그건 runtime behavior dial).

### (B) Pipeline 마지막 cmd subshell 여부 — 모드별 차등

```sh
echo "hello" | read x
echo $x       # POSIX/bash: "", ksh93: "hello"
```

| 모드 | 동작 |
|---|---|
| POSIX-strict, POSIX-aware | 마지막도 subshell (POSIX 명세) |
| ksh93u-strict, ksh93u-aware | 마지막은 현재 shell (ksh93) |
| default | **마지막은 현재 shell** (footgun 제거 정신) |
| `-secure` | 차이 없음 (변수 손실은 보안 이슈 아님) |

Runtime behavior dial은 모드별 차등 OK (이미 null glob, strict typing 등에서 같은 패턴).

### (C) Coprocess 문법 — 두 form 공존
```sh
# (i) ksh form — 익명, 한 번에 하나
cmd |&
print -p "input"; read -p output

# (ii) bash style — named, 동시 다수
coproc reader cat large.log
coproc filter grep ERROR
echo data >&${reader[1]}
read line <&${reader[0]}
```

ksh93u-strict/aware는 (i)만, 나머지는 둘 다.

### (D) Backticks `` `cmd` `` — deprecate but keep
- POSIX-strict/aware, ksh93u-strict/aware: 가용 (POSIX 정의)
- default: 가용 + `set -o warn-backticks` 옵션 (set options 결정에서 이름 확정)
- **`-secure`: 금지 (lock)** — set options 결정에서 확정
- `$(...)` 권장 (nested quoting 우수)

### (E) Process substitution `<(cmd)`, `>(cmd)` — 채택
- POSIX-strict: × (POSIX 미정의)
- 그 외 모두 ✓
- 구현: `/dev/fd/N` 가능하면 그쪽, 없으면 named pipe (FIFO)

### (F) Subshell 의미론 — 항상 POSIX (재확인)
ksh93의 subshell 최적화로 인한 관찰 가능한 부수효과 (`$RANDOM` 재시드 안 됨, `$$` 부모 PID 등)는 **모든 모드에서 POSIX 의미론 따름**. ksh93u-strict/aware도 예외 아님 — "POSIX 우선" 원칙 + "복각이 아니다" 원칙.

### (G) Background `&` — POSIX 그대로
- 기본 동작 POSIX
- `set -b` 옵션으로 immediate notification (bash 호환)
- 세부 job control은 별도 결정 layer

### (H) `wait` 확장
- bash 4.3+의 `wait -n` (any job 완료), `wait -p var` (PID 회수) 채택
- POSIX-aware 이상에서.

## 모드별 가용성 요약

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `(cmd)`, `$(cmd)`, backticks | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<(cmd)`, `>(cmd)` process subst | × | ✓ | ✓ | ✓ | ✓ |
| `\|&` coprocess (모든 모드 동일 의미) | × | ✓ | ✓ | ✓ | ✓ |
| `coproc name cmd` (multi) | × | ✓ | × | × | ✓ |
| Pipeline 마지막 = 현재 shell | × | × | ✓ | ✓ | ✓ |
| `wait -n`, `wait -p` | × | ✓ | × | × | ✓ |
| Backtick deprecation warning | × | × | × | × | ✓ (옵션) |

## 미결

남은 impl detail / v2+:
- Pipeline lastpipe 시 마지막 cmd의 SIGPIPE 처리 (bash 함정 회피 정책 — impl 시 신중)
- Process subst의 fd 자원 누수 진단 — `warn-fd-leak` option v2+

기타 모두 project_kash_sweep_v1.md에서 해소 — coproc fd `${name[0]}`/`${name[1]}` (bash 호환), warn-backticks 이름 그대로, `-secure` backtick 금지 확정.

**How to apply:** I/O redirection, job control, set option 등 후속 실행 layer 결정 시 위 가용성 표 형식 유지. `|&` 의미는 모든 모드에서 coprocess임을 잊지 말 것.
