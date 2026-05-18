# Job Control

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

**Jobspec**: `%n`, `%string`, `%?string`, `%%`, `%+`, `%-`.

## 채택할 확장

### `jobs` 출력 옵션
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

### `wait` 확장
- `wait -n` — any job 완료 대기
- `wait -p var` — 끝난 PID를 var에 저장
- `wait -f` — 진짜 종료까지 대기, suspended 무시

### `kill` 확장
- `kill -n NUM PID` — 신호 번호 (ksh93)
- `kill -l NUM` — 번호 → 이름 변환 (bash)
- `kill -L` — table 형식 (zsh)

## Monitor mode 기본 (POSIX 따름)

| 컨텍스트 | 기본 |
|---|---|
| Interactive shell | `monitor` on |
| Non-interactive script | `monitor` off |
| Subshell `(cmd)` | `monitor` off |
| 함수 호출 | 부모 상속 |

mode 무관. `-secure`도 동일.

## Leaky-jobs 3-option 패밀리 (신규)

Script 종료 시 wait/disown되지 않은 background job 존재 시 동작.

| Option | 동작 |
|---|---|
| `warn-leaky-jobs` | stderr warning, 정상 종료 |
| `ask-leaky-jobs` | interactive면 prompt (kill/disown/wait/proceed), non-interactive면 error로 격상 |
| `error-leaky-jobs` | exit 거부 또는 non-zero exit |

**모두 long-form only.**

### MX 규칙

**셋 중 둘 이상이 동시 on이면 kash 시작 자체 거부** (parse/init 단계 error). 의미 충돌 silent 방치 안 함.

### 모드별 기본

| 옵션 | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|
| `warn-leaky-jobs` | off | off | off | off | × (lock off) |
| `ask-leaky-jobs` | off | off | off | off | × (lock off) |
| `error-leaky-jobs` | off | off | off | off | **on (lock)** |

`-secure`는 가장 strict한 `error-leaky-jobs` lock on. 다른 둘은 lock off (MX).

### `-secure` 진입 시 conflict 처리

사용자가 명시적으로 `set -o warn-leaky-jobs` 한 뒤 `-secure` scope 진입 시 → **error**. 옵션 정리 후 진입해야 함. modifier monotonicity 원칙 일관.

## Async/await syntax — 영구 보류

[별도 정책](#): POSIX 최신 개정판에 정식 포함되기 전까지 구현 검토 자체 안 함.

POSIX `&` + `wait` + `wait -p var` 조합으로 사실상 충분.

## 결과 캡처 (기존 primitive)

```sh
# 임시 파일
cmd > out.tmp &; pid=$!; wait $pid && result=$(<out.tmp)

# coprocess
coproc result cmd args
wait $result_PID
read -d $'\0' output <&${result[0]}
```

새 syntax 없음. 패턴 좋아지면 `parallel-run` 같은 first-party utility로.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX jobs/fg/bg/wait/kill/disown | ✓ | ✓ | ✓ | ✓ | ✓ |
| Jobspec | ✓ | ✓ | ✓ | ✓ | ✓ |
| `jobs -l/-p/-r/-s/-n` | × | ✓ | ✓ | ✓ | ✓ |
| `disown -h/-a/-r` | × | ✓ | ✓ | ✓ | ✓ |
| `wait -n/-p/-f` | × | ✓ | × | ✓ | ✓ |
| `kill -n/-l NUM/-L` | × | ✓ | 일부 (-n, -l) | ✓ | ✓ |
| `warn/ask/error-leaky-jobs` (MX) | × | ✓ | × | ✓ | ✓ |

## 미결

- `ask-leaky-jobs`의 prompt UI 형식 (line editor 결정과 연동)
- `error-leaky-jobs` 시 정확한 exit code
- `wait -f`와 SIGTSTP의 race condition 처리
- non-interactive에서 `ask-` → `error-` 격상 시 stderr 메시지 형식
- coprocess가 leaky-job 카테고리에 포함되는지 (현재 가정: 포함)
