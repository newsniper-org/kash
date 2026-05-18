# Subshell, Pipeline, Coprocess, Process Substitution

kash 실행 layer의 첫 결정.

## (A) `|&` = coprocess (모든 모드)

ksh93 baseline 따름. bash/zsh의 stderr+stdout pipe 의미 거부.

- `|&` → coprocess (ksh93)
- stderr+stdout pipe → `cmd 2>&1 | cmd2` 명시적 표기
- bash/zsh muscle memory 사용자에게 known incompatibility로 문서화

구문 의미는 모드 무관 동일.

## (B) Pipeline 마지막 cmd subshell 여부 — 모드별 차등

```sh
echo "hello" | read x
echo $x       # POSIX/bash: "", ksh93: "hello"
```

| 모드 | 동작 |
|---|---|
| POSIX-strict, POSIX-aware | 마지막도 subshell (POSIX 명세) |
| ksh93u-strict, ksh93u-aware | 마지막은 현재 shell (ksh93) |
| default | **마지막은 현재 shell** (footgun 제거) |
| `-secure` | 차이 없음 |

Runtime behavior dial이라 모드별 차등 OK.

## (C) Coprocess 문법 — 두 form 공존

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

## (D) Backticks — deprecate but keep

- 모든 모드에서 가용 (POSIX 정의)
- default 모드에 deprecation warning 옵션 (`set -o warn-backticks`, 구체 이름 미정)
- `-secure`: backticks 금지 검토
- `$(...)` 권장

## (E) Process substitution `<(cmd)`, `>(cmd)` — 채택

- POSIX-strict: × (POSIX 미정의)
- 그 외 모두 ✓
- 구현: `/dev/fd/N` 가능하면 그쪽, 없으면 named pipe (FIFO)

## (F) Subshell 의미론 — 항상 POSIX

ksh93의 subshell 최적화로 인한 관찰 가능한 부수효과 (`$RANDOM` 재시드 안 됨, `$$` 부모 PID 등)는 **모든 모드에서 POSIX 의미론**. ksh93u-strict/aware도 예외 아님.

## (G) Background `&` — POSIX 그대로

- 기본 POSIX
- `set -b`로 immediate notification (bash 호환)
- 세부 job control은 별도 layer

## (H) `wait` 확장

bash 4.3+의 `wait -n`, `wait -p var` 채택. POSIX-aware 이상에서.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `(cmd)`, `$(cmd)`, backticks | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<(cmd)`, `>(cmd)` process subst | × | ✓ | ✓ | ✓ | ✓ |
| `\|&` coprocess | × | ✓ | ✓ | ✓ | ✓ |
| `coproc name cmd` (multi) | × | ✓ | × | × | ✓ |
| Pipeline 마지막 = 현재 shell | × | × | ✓ | ✓ | ✓ |
| `wait -n`, `wait -p` | × | ✓ | × | × | ✓ |
| Backtick deprecation warning | × | × | × | × | ✓ (옵션) |

## 미결

- `coproc`의 정확한 fd naming convention
- `warn-backticks` 옵션의 정확한 이름
- `-secure`에서 backtick 금지 여부 확정
- Pipeline 마지막 cmd가 현재 shell일 때 SIGPIPE 처리
- Process subst의 fd 자원 누수 패턴 처리
