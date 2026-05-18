---
name: kash — job control (committed)
description: POSIX job control + bash/ksh93 확장, `wait -f` 채택, leaky-jobs 3-option 패밀리 (warn/ask/error 상호 배타), monitor mode 기본
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash job control 시스템 확정 사항. (관련: project_shell_subshell_pipeline.md, project_shell_trap_signal.md, project_shell_set_options.md)

## POSIX 기반 (모든 모드)

```sh
cmd &                       # background
Ctrl-Z                       # SIGTSTP, suspend
jobs                         # 목록
fg [%jobspec]                # foreground 복귀
bg [%jobspec]                # background 계속
wait [PID|%jobspec]          # 완료 대기
kill [-SIGNAL] %jobspec      # signal 전송
disown [%jobspec]            # job table에서 제거
set -m / set -o monitor      # job control 활성
```

Jobspec: `%n`, `%string`, `%?string`, `%%`, `%+`, `%-`.

## 채택할 확장

### `jobs` 출력 옵션 (bash)
| Flag | 의미 |
|---|---|
| `-l` | long format (PID 포함) |
| `-p` | PID만 |
| `-r` | running만 |
| `-s` | stopped만 |
| `-n` | 마지막 notify 이후 변경된 것만 |

### `disown` 확장
| Flag | 의미 |
|---|---|
| `-h` | job 유지하되 SIGHUP 안 보냄 |
| `-a` | 모든 job |
| `-r` | running만 |

### `wait` 확장 (subshell/pipeline 결정 + 추가)
- `wait -n` — any job 완료 대기 (bash 4.3+)
- `wait -p var` — 끝난 PID를 var에 저장 (bash 5.1+)
- `wait -f` — 진짜 종료까지 대기, suspended 무시 (bash 5.1+) — **신규 채택**

### `kill` 확장
- `kill -n NUM PID` — 신호 번호 (ksh93)
- `kill -l NUM` — 번호 → 이름 변환 (bash)
- `kill -L` — table 형식 (zsh — 유용)

## Monitor mode 기본 — POSIX 따름

| 컨텍스트 | 기본 |
|---|---|
| Interactive shell | `monitor` on |
| Non-interactive script | `monitor` off |
| Subshell `(cmd)` | `monitor` off |
| 함수 호출 | 부모 상속 |

mode와 무관 — POSIX 규칙. `-secure`도 동일.

## SIGCHLD와 trap 상호작용

[trap memory](project_shell_trap_signal.md)와 정합. SIGCHLD trap으로 job state 변경 catch 가능.

## Leaky-jobs 3-option 패밀리 (신규)

Script 종료 시 wait/disown되지 않은 background job 존재 시 동작 정책.

| Option | 동작 |
|---|---|
| `warn-leaky-jobs` | stderr에 warning, 정상 종료 |
| `ask-leaky-jobs` | interactive면 prompt (kill/disown/wait/proceed 선택), non-interactive면 error로 격상 |
| `error-leaky-jobs` | exit 거부 (또는 non-zero exit), leaky job을 error로 취급 |

**전부 long-form only** (set options 컨벤션 일관).

### 상호 배타 (MX) 규칙
**세 옵션 중 둘 이상이 동시 on이면 kash 시작 자체를 거부** (parse/init 단계 error). 의미 충돌 silent 방치 안 함.

### 기본 상태 (모드별)
| 옵션 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| `warn-leaky-jobs` | n/a | off | off | off | off | × (lock off) |
| `ask-leaky-jobs` | n/a | off | off | off | off | × (lock off) |
| `error-leaky-jobs` | n/a | off | off | off | off | **on (lock)** |

`-secure`는 가장 strict한 `error-leaky-jobs` lock on. 다른 두 옵션은 *lock off* (상호 배타이므로).

### `-secure` 진입 시 conflict 처리
사용자가 명시적으로 `set -o warn-leaky-jobs` 한 뒤 `-secure` scope 진입 시:
- `-secure`가 `error-leaky-jobs` on을 lock하려는데 `warn-leaky-jobs`가 이미 on → MX 위반
- → **error** (사용자가 옵션 정리 후 진입해야 함)

modifier monotonicity 원칙 일관 — `-secure`는 안전성을 *낮출 수 없음*. 충돌 시 무조건 reject.

## Async/await syntax — 영구 보류

별도 feedback (feedback_no_async_await_until_posix.md): **POSIX 최신 개정판에 포함되기 전까지 구현 검토조차 안 함**. POSIX `&` + `wait` + `wait -p var` 조합으로 사실상 충분, invention 비용 큼.

## 결과 캡처 — 기존 primitive로 충분

```sh
# 임시 파일
cmd > out.tmp &; pid=$!; wait $pid && result=$(<out.tmp)

# coprocess
coproc result cmd args
wait $result_PID
read -d $'\0' output <&${result[0]}
```

새 syntax 없이 가능. 패턴 자체 좋아지면 `parallel-run` 같은 first-party utility로.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX jobs/fg/bg/wait/kill/disown | ✓ | ✓ | ✓ | ✓ | ✓ |
| Jobspec (`%n`, `%string` 등) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `jobs -l/-p/-r/-s/-n` | × | ✓ | ✓ | ✓ | ✓ |
| `disown -h/-a/-r` | × | ✓ | ✓ | ✓ | ✓ |
| `wait -n/-p/-f` | × | ✓ | × | ✓ | ✓ |
| `kill -n/-l NUM/-L` | × | ✓ | 일부 (-n, -l) | ✓ | ✓ |
| `warn/ask/error-leaky-jobs` (MX) | × | ✓ | × | ✓ | ✓ |

## 미결

남은 impl detail:
- `wait -f`와 SIGTSTP race condition 정확한 처리 (impl)

기타 모두 project_kash_sweep_v1.md에서 해소 — prompt `[k]ill/[d]isown/[w]ait/[p]roceed?`, exit code `3`, coprocess leaky-job 포함.

**How to apply:** signal/job 관련 후속 결정 시 위 가용성 표 유지. 새 footgun 발견 시 `warn-*`/`ask-*`/`error-*` 3-tier 패밀리 패턴 고려 (leaky-jobs의 모델).
