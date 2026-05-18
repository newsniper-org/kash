---
name: kash — set options (committed)
description: POSIX set options + ksh/bash 확장 + 신규 warn-* + -secure modifier가 강제 on/lock하는 항목 확정
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash의 글로벌 토글 (`set` options) 시스템 확정 사항. (관련: project_shell_modes.md, project_shell_subshell_pipeline.md, project_shell_glob_pattern.md, project_shell_expansion_flags.md)

## POSIX core (모든 모드 지원)

`-a allexport`, `-b notify`, `-C noclobber`, `-e errexit`, `-f noglob`, `-h hashall`, `-m monitor`, `-n noexec`, `-u nounset`, `-v verbose`, `-x xtrace`.

## 채택할 ksh93/bash 확장

| Long | 출처 | 의미 |
|---|---|---|
| `pipefail` | ksh93/bash/zsh | 파이프라인 종료코드 = rightmost non-zero |
| `errtrace` (`-E`) | bash | ERR trap이 함수/서브쉘로 상속 |
| `functrace` (`-T`) | bash | DEBUG/RETURN trap 상속 |
| `ignoreeof` | ksh93/bash/zsh | Ctrl-D로 종료 방지 (interactive) |
| `nolog` | ksh93/bash | 함수 정의를 history에 추가 안 함 |

## 인터페이스 통합

- **`set -o NAME` 단일 인터페이스** — bash의 `shopt` 분리 거부. shopt-only였던 옵션들(`globstar`, `nocaseglob` 등)도 모두 `set -o`로 통합.
- **`setopt NAME` alias** — zsh 호환을 위해 alias만 제공. 신규 코드는 `set -o` 권장. transpiler가 zsh `setopt` → kash `set -o` 매핑.

## 신규 kash-specific 옵션 (4 + 3 = 7종)

### 단일 옵션 (4종)
| Long form | 의미 | 도입 동기 |
|---|---|---|
| `warn-backticks` | `` `...` `` 사용 시 warning | subshell 결정 |
| `warn-unsafe-eval` | `${(e)var}` 사용 시 warning | expansion flags 결정 |
| `warn-implicit-array` | indexed array에 string key 인덱싱 시 warning | array footgun 보존 시 보조 |
| `warn-leaky-glob` | null glob unchanged 시 warning (모드 차이 인식 보조) | glob 결정 |

### Leaky-jobs 3-option 패밀리 (상호 배타 MX)
| Long form | 의미 |
|---|---|
| `warn-leaky-jobs` | 종료 시 미정리 bg job 있으면 warning, 정상 종료 |
| `ask-leaky-jobs` | interactive면 prompt, non-interactive면 error로 격상 |
| `error-leaky-jobs` | exit 거부 또는 non-zero exit |

**MX 규칙**: 셋 중 둘 이상이 동시 on이면 kash 시작 자체 거부 (parse/init 단계 error).

**long-form only** (모든 신규 옵션) — 단축 letter는 POSIX 호환 위해 reserve.

## `-secure` modifier 강제 on/lock 항목 (확정)

| 항목 | 효과 |
|---|---|
| `errexit` | **강제 on (lock)** |
| `pipefail` | **강제 on (lock)** |
| `nounset` | **강제 on (lock)** |
| `noclobber` | **강제 on (lock)** — `>|` 명시적 override는 여전히 허용 |
| Null glob → fail | **강제 (lock)** |
| `(e)` re-eval | **금지 (lock)** |
| Backticks | **금지 (lock)** — 이전 미결, 여기서 확정 |
| `warn-*` 4종 (backticks/unsafe-eval/implicit-array/leaky-glob) | **모두 강제 on (lock)** |
| Leaky-jobs 3-option | **`error-leaky-jobs` on (lock)**, 다른 두 개는 off lock |
| `eval` builtin | **금지 (lock)** — Shellshock 정책 P3 적용 (project_kash_security_policy.md) |

### Modifier monotonicity 적용
`-secure` scope 내에서 위 옵션 끄려는 시도 (`set +e`, `set +o pipefail` 등) → **error**. 안전성 silent 풀림 방지.

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

default 모드는 POSIX 기본 유지 (호환성). 한번에 너무 많이 켜면 ksh93/bash 스크립트 silent 깨짐 — 안전성은 `-secure` 옵트인.

## IFS

POSIX 변수, `set` 옵션 아님. 기본값 = 공백/탭/개행. `-secure`도 IFS lock 안 함 (정당한 사용 다수).

## 미결

남은 impl detail:
- `-secure` lock 옵션 명시 해제 시도 시 정확한 error 메시지/exit code (impl)

기타 모두 project_kash_sweep_v1.md에서 해소 — warn-* 출력 stderr, `set -o` POSIX+bash 호환 형식, 모드 전환 시 lexical 자동 복원, ksh93u에서 warn-unsafe-eval n/a 이유는 doc-only.

**How to apply:** 신규 footgun 후보 발견 시 `warn-*` 옵션 추가 검토. `-secure` 강화 필요한 항목 누적되면 위 표 갱신. 새 modifier (`-no-network`, `-no-glob` 등) 도입 시 비슷한 강제 on/lock 표 형식으로 정리.
