# Prompt System

## Canonical: function-based (fish 노선)

Prompt 생성을 *함수*가 담당. PS1 문자열 escape sequence model은 transpiler가 호환 처리.

### Well-known functions (`.kash.*`)

| 함수 | 역할 | 호출 시점 |
|---|---|---|
| `.kash.prompt` | 주 prompt 출력 | 매 prompt 직전 |
| `.kash.right_prompt` | 우측 prompt (RPROMPT 대체) | 매 prompt 직전 |
| `.kash.continuation_prompt` | 다중 라인 continuation | continuation 시 |
| `.kash.select_prompt` | `select` builtin prompt | `select` 실행 |
| `.kash.xtrace_prefix` | `set -x` trace 접두 | xtrace 출력마다 |
| `.kash.precmd` | prompt 직전 hook | 매 명령 실행 후 |
| `.kash.preexec` | 명령 실행 직전 hook | 매 사용자 명령 전 |
| `.kash.chpwd` | PWD 변경 hook | `cd`/`pushd`/`popd` 후 |

함수 미정의면 기본 prompt (`$ ` 또는 `# `).

### 예시
```kash
function .kash.prompt {
    printf '%s@%s %s %s ' "$USER" "${HOSTNAME%%.*}" "${PWD/#$HOME/~}" '$'
}

function .kash.right_prompt {
    printf '[%s]' "$(date '+%H:%M')"
}
```

## PS1/PS2/PS3/PS4 — compat fallback

- 변수 존재, `.kash.*` 함수 없을 때만 fallback
- 함수 있으면 함수 우선 (canonical)
- `PS0` (post-read pre-exec, bash 4.4+) 채택

## bash/zsh escape — transpiler 처리

- bash `PS1='\u@\h \w \$ '` → kash `function .kash.prompt { ... }` 변환
- zsh `PS1='%n@%m %~ %# '` → kash 함수
- bash `PROMPT_COMMAND='...'` → kash `function .kash.precmd { ... }`
- zsh `precmd() { ... }` → kash `function .kash.precmd { ... }`

별도 도구 없음 — script transpiler가 PS1/PROMPT_COMMAND 패턴 인식.

## 색/스타일

`term-style` utility로 별도 carve out (다음 first-party utility round, 미확정).

## precmd vs DEBUG trap (별개)

| | 호출 시점 |
|---|---|
| `DEBUG` trap | 모든 simple command 직전 |
| `.kash.preexec` | 사용자 명령 실행 직전 (interactive) |
| `ERR` trap | 명령 non-zero exit 후 |
| `.kash.precmd` | prompt 출력 직전 |

## prompt 안 expansion

함수 내에서 kash의 모든 expansion 사용 가능 — expansion flags, compound var, discipline getter, 모드별 분기.

## Caching 책임

`.kash.prompt`는 매 prompt마다 호출. expensive 작업은 사용자 책임 (fish 패턴).

## Non-interactive

prompt hook은 interactive에서만. `.kash.xtrace_prefix`만 non-interactive `set -x`에서도.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `PS1/PS2/PS4` 변수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS3` (select) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS0` | × | ✓ | × | ✓ | ✓ |
| `.kash.prompt` etc. | × | ✓ | × | ✓ | ✓ |

## 미결

- `.kash.prompt` 출력 vs 변수 채우기 (현재: stdout)
- Multi-line prompt (rustyline 지원 / 포크)
- `.kash.preexec` 발동 scope
- `term-style` interface
- last exit code 노출 변수
- VI mode indicator
