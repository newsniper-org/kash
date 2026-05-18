---
name: kash — prompt system (committed)
description: fish-style canonical function-based prompt, `.kash.*` well-known hooks, PS1 compat, bash/zsh escape는 transpiler 처리
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash prompt 시스템 확정 사항. (관련: project_kash_interactive.md, project_shell_transpiler.md, project_shell_trap_signal.md)

## Canonical: function-based (fish 노선)

Prompt 생성을 *함수*가 담당. PS1 문자열에 escape sequence 끼워넣는 model은 transpiler가 호환 처리.

### Well-known function 이름 (`.kash.*` namespace)

| 함수 | 역할 | 호출 시점 |
|---|---|---|
| `.kash.prompt` | 주 prompt 출력 | 매 prompt 직전 |
| `.kash.right_prompt` | 우측 prompt (RPROMPT 대체) | 매 prompt 직전 (rustyline 지원 시) |
| `.kash.continuation_prompt` | 다중 라인 입력 continuation | continuation 필요 시 |
| `.kash.select_prompt` | `select` builtin prompt | `select` 실행 시 |
| `.kash.xtrace_prefix` | `set -x` trace 접두 | xtrace 출력마다 |
| `.kash.precmd` | prompt 출력 직전 hook | 매 명령 실행 후 |
| `.kash.preexec` | 명령 실행 직전 hook | 매 사용자 명령 실행 전 (DEBUG trap과 별개) |
| `.kash.chpwd` | PWD 변경 시 hook | `cd`/`pushd`/`popd` 후 |

함수 미정의면 기본 prompt (`$ ` 또는 `# ` for root).

### 예시
```kash
function .kash.prompt {
    printf '%s@%s %s %s ' "$USER" "${HOSTNAME%%.*}" "${PWD/#$HOME/~}" '$'
}

function .kash.right_prompt {
    printf '[%s]' "$(date '+%H:%M')"
}
```

## PS1/PS2/PS3/PS4 변수 — compat용 fallback 유지

POSIX/bash/ksh/zsh 호환:
- `PS1`, `PS2`, `PS3`, `PS4` 변수 존재
- `.kash.prompt` 등 함수가 *없을 때만* fallback
- 함수 있으면 함수 우선 (canonical)
- `PS0` (post-read pre-exec) — bash 4.4+ 채택

## bash/zsh escape 처리 — transpiler 일임

runtime shim 없음:
- bash `PS1='\u@\h \w \$ '` → kash `function .kash.prompt { ... }` 변환된 함수 body
- zsh `PS1='%n@%m %~ %# '` → kash 함수
- bash `PROMPT_COMMAND='...'` → kash `function .kash.precmd { ... }`
- zsh `precmd() { ... }` → kash `function .kash.precmd { ... }`

별도 transpiler 도구 *없음* — 위 *script transpiler*가 PS1/PROMPT_COMMAND 패턴 인식 (`.bashrc`/`.zshrc`의 일부로 처리).

## 색/스타일 — first-party utility로 (별도 commit 예정)

```kash
# 잠정 (commit 전 — stability rule 적용)
term-style red bold
print "$(term-style red)error$(term-style reset)"
```

`term-style`은 별도 카테고리 utility — 다음 first-party utility 결정 round에서 lock. fish `set_color`, zsh `%F{}` 대응.

## precmd vs DEBUG trap — 별개 메커니즘

| | 호출 시점 | 용도 |
|---|---|---|
| `DEBUG` trap | *모든* simple command 직전 (subshell, builtin 포함) | low-level 디버깅 |
| `.kash.preexec` | *사용자 명령* 실행 직전 | interactive hook (prompt UX) |
| `ERR` trap | 명령 non-zero exit 후 즉시 | 에러 처리 |
| `.kash.precmd` | prompt 출력 직전 (다음 명령 받기 전) | prompt 갱신 hook |

별개로 다룸. precmd는 interactive prompt 사이클의 일부, DEBUG/ERR는 명령 실행 사이클의 일부.

## prompt 표현 내 expansion

함수 내에서 kash의 모든 expansion 사용 가능:
- expansion flags: `${(L)var}`, `${(j:/:)pathseg}`
- compound var: `${.sh.mode}`, `${.kash.git.branch}` (가상)
- discipline getter: 매 호출마다 최신 값
- 모드별 다른 prompt 가능 (`mode -L` 안에서 별도 함수)

## 빈도 / caching 책임

- `.kash.prompt`는 매 prompt마다 호출 → expensive 작업 (git status, network 등)은 사용자가 직접 caching
- fish 검증 패턴

## Non-interactive

- prompt 관련 hook은 interactive에서만 호출 (`.kash.prompt`, `.kash.right_prompt`, `.kash.precmd`, `.kash.preexec`, `.kash.chpwd`)
- Non-interactive에서는 호출 없음, PS1 변수도 무관
- `.kash.xtrace_prefix`는 non-interactive에서도 `set -x` 사용 시 호출

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `PS1`, `PS2`, `PS4` 변수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS3` (select) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS0` (post-read pre-exec) | × | ✓ | × | ✓ | ✓ |
| `.kash.prompt` 함수 canonical | × | ✓ | × | ✓ | ✓ |
| `.kash.right_prompt` | × | ✓ | × | ✓ | ✓ |
| `.kash.precmd`/`.preexec`/`.chpwd` | × | ✓ | × | ✓ | ✓ |
| bash escape in PS1 (\u, \h 등) | POSIX impl-defined | ✓ (호환) | × | × | × (transpiler 권장) |

## 미결

남은 v2+/impl 항목:
- Multi-line prompt — rustyline 18.0.0 fork에서 patch (v2+)
- VI mode indicator — rustyline fork 시 추가 (v2+)
- Last exit code 노출 변수 — `$?` POSIX 유지 + `${.sh.last_exit_code}` alias 검토 (impl 또는 별도 결정)

기타 모두 project_kash_sweep_v1.md에서 해소 — stdout, top-level only, term-style은 별도 lock.

**How to apply:** 인터랙티브 layer 후속 결정 (history, abbreviation 등) 시 `.kash.*` namespace 일관 유지. fish-style hook 패턴 follow.
