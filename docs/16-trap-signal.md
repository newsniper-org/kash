# Trap and Signal Handling

## POSIX baseline

```sh
trap 'CMD' SIGNAL [SIGNAL...]    # set handler
trap - SIGNAL                     # reset to default
trap '' SIGNAL                    # ignore
trap                              # print current traps
trap -p                           # machine-readable
```

POSIX signals + `EXIT` pseudo-signal. 모든 모드.

## Pseudo-signals (확장)

| 이름 | 의미 |
|---|---|
| `EXIT` (= `0`) | 셸 종료 시 |
| `DEBUG` | 각 명령 실행 전 |
| `ERR` | 명령 non-zero exit 후 |
| `RETURN` | 함수/sourced script return 시 |

상속은 [set options](15-set-options.md)의 `errtrace`/`functrace`로 통제.

## Trap stacking — kash 신규

표준 trap의 덮어쓰기 footgun 해결:

```sh
trap 'CMD' SIGNAL              # POSIX — 교체
trap --append 'CMD' SIGNAL     # 기존 뒤에 추가
trap --prepend 'CMD' SIGNAL    # 기존 앞에 추가
trap --list-stack SIGNAL       # 등록된 모든 핸들러 출력
trap --clear SIGNAL            # 모든 stacked 핸들러 제거
```

등록 순서로 실행. ERR trap exit code = 첫 번째 실패한 핸들러.

POSIX-strict / ksh93u-strict는 비활성.

## `.sh.*` context 변수

| 변수 | 의미 | 트랩 |
|---|---|---|
| `.sh.signal` | signal name | 모든 signal trap |
| `.sh.exit_status` | 트리거 exit code | ERR |
| `.sh.line` | 현재 source line | DEBUG, ERR |
| `.sh.command` | 실행 중/직전 명령 | DEBUG, ERR |
| `.sh.funcname[]` | 함수 호출 stack | 모든 트랩 |
| `.sh.source[]` | source file stack | 모든 트랩 |
| `.sh.subshell_level` | subshell nesting | 모든 트랩 |

bash 호환은 transpiler가:
- `LINENO` → `${.sh.line}`
- `BASH_COMMAND` → `${.sh.command}`
- `FUNCNAME` → `${.sh.funcname}`
- `BASH_SOURCE` → `${.sh.source}`

## `-secure`와 ERR trap

`-secure`는 ERR trap을 강제하지 *않음*. `errexit`이 이미 lock이라 자체 fail-fast. ERR trap 설치는 사용자 선택 (로깅/cleanup용).

향후 `-traceable` 같은 별도 modifier로 검토 가능.

## Safety trap monotonicity — 부분 적용

- **Set option** (errexit, pipefail 등): `-secure` 안에서 끄기 금지 (option monotonicity)
- **User-installed trap** (ERR, EXIT 등): inner scope 제거/교체 허용 (사용자 책임)

## Introspection

```sh
trap                       # POSIX human-readable
trap -p                    # POSIX 재실행 form
trap -p SIGNAL             # 특정 signal
trap -l                    # signal 이름 리스트 (bash 확장)
trap --list-stack SIGNAL   # stacked 핸들러 (kash 신규)
```

## Signal masking — v1 보류

POSIX `sigprocmask` 노출 안 함. `trap '' SIGNAL`로 충분한 경우 다수. 향후 v2.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX `trap` + EXIT | ✓ | ✓ | ✓ | ✓ | ✓ |
| DEBUG/ERR/RETURN | × | ✓ | ✓ | ✓ | ✓ |
| `trap -p` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `trap -l` | × | ✓ | × | ✓ | ✓ |
| Trap stacking | × | ✓ | × | ✓ | ✓ |
| `.sh.*` context vars | × | ✓ | ✓ | ✓ | ✓ |

## 미결

- ERR trap이 errexit으로 인한 exit *전*에 실행 (권장)
- DEBUG trap이 builtin 명령에도 발동되는지
- Trap handler 안 `.sh.exit_status` 변경 정책
- coproc 종료 signal 시퀀스 (SIGTERM → grace → SIGKILL)
- `trap '' SIGNAL` 무시 설정의 child 전파 POSIX 규칙
- Trap stacking 중복 추가 idempotency 옵션
