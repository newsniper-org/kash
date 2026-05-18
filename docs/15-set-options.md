# Set Options

셸 동작 글로벌 토글.

## POSIX core (모든 모드)

`-a allexport`, `-b notify`, `-C noclobber`, `-e errexit`, `-f noglob`, `-h hashall`, `-m monitor`, `-n noexec`, `-u nounset`, `-v verbose`, `-x xtrace`.

## 채택할 ksh93/bash 확장

| Long | 출처 | 의미 |
|---|---|---|
| `pipefail` | ksh93/bash/zsh | 파이프라인 종료코드 = rightmost non-zero |
| `errtrace` (`-E`) | bash | ERR trap이 함수/서브쉘 상속 |
| `functrace` (`-T`) | bash | DEBUG/RETURN trap 상속 |
| `ignoreeof` | ksh93/bash/zsh | Ctrl-D 종료 방지 (interactive) |
| `nolog` | ksh93/bash | 함수 정의 history 추가 안 함 |

## 인터페이스 통합

- **`set -o NAME` 단일 인터페이스** — bash의 `shopt` 분리 거부. 모든 옵션을 `set -o`로.
- **`setopt NAME` alias** — zsh 호환 위해 alias만 제공. 신규 코드는 `set -o`.

## 신규 kash-specific 옵션 (4 + 3 = 7종)

모두 long-form only.

### 단일 옵션 (4종)
| Long form | 의미 |
|---|---|
| `warn-backticks` | `` `...` `` 사용 시 warning |
| `warn-unsafe-eval` | `${(e)var}` 사용 시 warning |
| `warn-implicit-array` | indexed array에 string key 인덱싱 시 warning |
| `warn-leaky-glob` | null glob unchanged 시 warning |

### Leaky-jobs 3-option 패밀리 (상호 배타 MX)
| Long form | 의미 |
|---|---|
| `warn-leaky-jobs` | 종료 시 미정리 bg job 있으면 warning |
| `ask-leaky-jobs` | interactive면 prompt, non-interactive면 error 격상 |
| `error-leaky-jobs` | exit 거부 / non-zero exit |

**MX**: 셋 중 둘 이상이 동시 on이면 kash 시작 거부.

## `-secure` modifier 강제 on/lock

| 항목 | 효과 |
|---|---|
| `errexit` | 강제 on (lock) |
| `pipefail` | 강제 on (lock) |
| `nounset` | 강제 on (lock) |
| `noclobber` | 강제 on (lock) — `>\|` override는 여전히 허용 |
| Null glob → fail | 강제 (lock) |
| `(e)` re-eval | 금지 (lock) |
| Backticks | 금지 (lock) |
| `warn-*` 4종 (backticks/unsafe-eval/implicit-array/leaky-glob) | 모두 강제 on (lock) |
| Leaky-jobs (MX 3종) | `error-leaky-jobs` on (lock), `warn-`/`ask-`는 off lock |

`-secure` scope 내에서 위 옵션 끄려는 시도 (`set +e` 등) → **error** (modifier monotonicity).

## 모드별 기본 상태

| 옵션 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| `errexit` | off | off | off | off | off | **on (lock)** |
| `pipefail` | off | off | off | off | off | **on (lock)** |
| `nounset` | off | off | off | off | off | **on (lock)** |
| `noclobber` | off | off | off | off | off | **on (lock)** |
| Null glob | unchanged | unchanged | unchanged | unchanged | **fail** | **fail (lock)** |
| `warn-backticks` | n/a | off | off | off | off | **on (lock)** |
| `warn-unsafe-eval` | n/a | off | n/a | off | off | **on (lock)** |
| `warn-implicit-array` | n/a | off | n/a | off | off | **on (lock)** |
| `warn-leaky-glob` | n/a | off | off | off | off | **on (lock)** |
| `warn-leaky-jobs` | n/a | off | off | off | off | × (lock off) |
| `ask-leaky-jobs` | n/a | off | off | off | off | × (lock off) |
| `error-leaky-jobs` | n/a | off | off | off | off | **on (lock)** |
| `(e)` 사용 가능 | × | ✓ | × | ✓ | ✓ | **× (lock)** |
| Backticks 사용 가능 | ✓ | ✓ | ✓ | ✓ | ✓ | **× (lock)** |

default 모드는 POSIX 기본 유지 (호환성). 안전성은 `-secure` 옵트인.

## IFS

POSIX 변수, `set` 옵션 아님. 기본값 = 공백/탭/개행. `-secure`도 IFS는 lock 안 함.

## 미결

- `warn-*` 옵션 warning 출력 위치 (stderr 기본)
- `set -o` 단독 호출 시 출력 형식
- 모드 간 전환 시 옵션 상태 보존/리셋 정책
- `-secure` lock 옵션 해제 시도 시 정확한 error 메시지/exit code
