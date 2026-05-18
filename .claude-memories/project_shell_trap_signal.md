---
name: kash — trap and signal handling (committed)
description: POSIX trap + pseudo-signals (EXIT/DEBUG/ERR/RETURN), trap stacking 신규 도입, .sh.* context vars, errtrace/functrace 상호작용
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash trap/signal 시스템 확정 사항. (관련: project_shell_set_options.md, project_shell_subshell_pipeline.md, project_shell_compound_vars.md)

## POSIX `trap` baseline

```sh
trap 'CMD' SIGNAL [SIGNAL...]    # set handler
trap - SIGNAL                     # reset to default
trap '' SIGNAL                    # ignore
trap                              # print current traps
trap -p                           # machine-readable print
```

POSIX signals (HUP, INT, QUIT, TERM, USR1, USR2 등) + `EXIT` pseudo-signal. 모든 모드 지원.

## Pseudo-signals (셸 확장 — 채택)

| 이름 | 의미 |
|---|---|
| `EXIT` (= `0`) | POSIX — 셸 종료 시 |
| `DEBUG` | 각 명령 실행 *전* |
| `ERR` | 명령 non-zero exit 후 |
| `RETURN` | 함수/sourced script return 시 |

상속 동작은 `errtrace`/`functrace` set option (project_shell_set_options.md):
- `errtrace` on: ERR이 함수/서브쉘 상속
- `functrace` on: DEBUG/RETURN이 함수 상속

POSIX-strict는 DEBUG/ERR/RETURN 비활성, 그 외 모든 모드 활성.

## Trap stacking — kash 신규 확장

표준 trap의 footgun (덮어쓰기) 해결.

```sh
trap 'CMD' SIGNAL              # POSIX — 기존 핸들러 *교체*
trap --append 'CMD' SIGNAL     # 신규: 기존 뒤에 추가
trap --prepend 'CMD' SIGNAL    # 신규: 기존 앞에 추가
trap --list-stack SIGNAL       # 신규: 등록된 모든 핸들러 출력
trap --clear SIGNAL            # 신규: 모든 stacked 핸들러 제거 (= `trap - SIGNAL`)
```

여러 핸들러 등록 시 등록 순서로 실행. ERR trap에서 exit code = *첫 번째 실패한* 핸들러의 exit code.

short letter 미배정 — POSIX `-` (reset)와 혼동 방지.

POSIX-strict / ksh93u-strict: stacking 비활성.

## `.sh.*` context 변수 (트랩 내 정보)

| 변수 | 의미 | 의미 있는 트랩 |
|---|---|---|
| `.sh.signal` | 트랩 발생 signal name | 모든 signal trap |
| `.sh.exit_status` | 트리거된 exit code | ERR |
| `.sh.line` | 현재 source line | DEBUG, ERR |
| `.sh.command` | 실행 중/직전 명령 문자열 | DEBUG, ERR |
| `.sh.funcname[]` | 함수 호출 stack | 모든 트랩 |
| `.sh.source[]` | 호출된 source file stack | 모든 트랩 |
| `.sh.subshell_level` | 현재 subshell nesting | 모든 트랩 |

bash 호환은 transpiler가 매핑:
- `LINENO` → `${.sh.line}`
- `BASH_COMMAND` → `${.sh.command}`
- `FUNCNAME` → `${.sh.funcname}`
- `BASH_SOURCE` → `${.sh.source}`

## `-secure`와 ERR trap — 강제 *안 함*

`-secure`는 이미 `errexit` lock. ERR trap 설치는 *선택* (사용자가 로깅/cleanup 원하면 직접). 강제 트랩 설치는 invasive — 보류. 향후 `-traceable` 같은 별도 modifier로 검토 가능.

## Safety trap의 monotonicity — 부분 적용

`-secure` scope에서:
- **Set option** (errexit, pipefail, nounset, noclobber 등): 끄기 금지 (이미 결정, 옵션 monotonicity)
- **User-installed trap** (ERR, EXIT 등): inner scope에서 *제거/교체 허용* (사용자 책임 영역)

즉 option은 monotonic, trap은 사용자 책임. 분리 정책.

## `trap` introspection 출력

```sh
trap              # human-readable (POSIX)
trap -p           # 재실행 가능 form (POSIX)
trap -p SIGNAL    # 특정 signal만
trap -l           # signal 이름 리스트 (bash 확장 — 채택)
trap --list-stack SIGNAL  # stacked 핸들러 표시 (kash 신규)
```

`trap -l`은 POSIX-aware 이상, `--list-stack`은 POSIX-aware 이상.

## Signal masking — v1 보류

POSIX `sigprocmask` 노출 안 함. 셸 사용자가 신호 차단할 일 드물고, 필요하면 `trap '' SIGNAL`로 충분. 향후 v2 검토.

## Coprocess signal 의미

- coproc은 separate process, 자체 handler
- kash 종료 시 coproc에 SIGTERM/SIGHUP 전달 (POSIX-ish)
- coproc fd 모두 닫히면 자연 종료 또는 SIGPIPE
- 세부는 OS 의존

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX `trap` + EXIT | ✓ | ✓ | ✓ | ✓ | ✓ |
| DEBUG/ERR/RETURN | × | ✓ | ✓ | ✓ | ✓ |
| `trap -p` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `trap -l` | × | ✓ | × | ✓ | ✓ |
| Trap stacking (`--append/-prepend/-list-stack/-clear`) | × | ✓ | × | ✓ | ✓ |
| `.sh.*` context vars in trap | × | ✓ | ✓ | ✓ | ✓ |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** signal/trap 후속 결정 (예: 향후 `-traceable` modifier, observability tooling) 시 위 context vars 명세를 일관 유지. `.sh.*` 추가 시 trap-only 인지 globally available인지 명시.
