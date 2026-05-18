---
name: kash — builtin command set (committed)
description: POSIX special+regular + ksh93/bash 확장, read --prompt 신설, echo/print/printf 정책, local=typeset, die/assert/usage 신규 builtin
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash builtin 명령 set 확정 사항. (관련: project_shell_set_options.md, project_shell_function_scope.md, project_shell_subshell_pipeline.md)

## POSIX baseline (모든 모드)

### Special builtins
`break`, `:`, `continue`, `.` (source), `eval`, `exec`, `exit`, `export`, `readonly`, `return`, `set`, `shift`, `trap`, `unset`.

### Regular builtins
`cd`, `echo`, `false`, `printf`, `pwd`, `read`, `test`/`[`, `true`, `umask`, `wait`, `getopts`, `command`, `times`, `alias`/`unalias`, `hash`, `type`, `select` (construct).

`source` 와 `.` 동기화 — 모든 모드에서 동의어.

## 채택할 ksh93 확장

| Builtin | 의미 |
|---|---|
| `print` | 일관된 출력 (echo 대체, canonical) |
| `print -r/-n/-u/-p` | raw/no-newline/fd/coproc 출력 |
| `whence [-vap]` | command resolution info |
| `let` | 산술 평가 (== `((...))`) — bash 호환 위해 POSIX-aware에도 |
| `compound`, `nameref` | `typeset -C`, `typeset -n`의 builtin alias |
| `redirect` | exec에 redirection 적용 |
| `time` (keyword + builtin) | timing |
| `enum` | enumerated type (ksh93 후기 기능) |

## 채택할 bash 확장

| Builtin | 의미 |
|---|---|
| `mapfile` / `readarray` | 파일/stdin을 배열로 |
| `pushd` / `popd` / `dirs` | 디렉토리 스택 |
| `caller` | 호출 stack 정보 (debug) — `.sh.callstack[]`도 함께 제공 |
| `help` | 내장 builtin 도움말 |
| `builtin` | 특정 builtin 강제 실행 |

## 거부

- **`enable -n`** (builtin 비활성화) — 위험, 거의 안 쓰임.
- **`complete`/`compgen`** — completion 시스템은 별도 layer (line editor 결정에서, 미결).
- **`bind`** — readline 바인딩은 별도 layer.

## 충돌 해소

### `read -p` (ksh93 coproc vs bash prompt)
- **ksh93 의미 유지**: `read -p` = coproc 읽기 (ksh93 baseline 원칙)
- **Prompt 기능은 long-form 신설**: `read --prompt "string" var`
- Transpiler가 bash `read -p "X" var` → `read --prompt "X" var` 매핑

### `echo` 정책
- POSIX-strict: POSIX 의미 (escape 없음, `-n` 미지원)
- POSIX-aware: implementation-defined
- ksh93u-strict/aware: ksh93 의미 + `print` 함께 가용
- default: `echo` 가용, **`warn-echo-portability` 옵션** 권장 (warn-* 패밀리 합류 검토)
- 모든 모드: 진지한 출력은 `printf` 권장 (canonical)

### `local` = `typeset` alias
- `local` builtin은 `typeset`의 alias (POSIX-aware 이상)
- POSIX form `f() { local x=1; ... }`: dynamic local (bash 호환)
- ksh form `function f { local x=1; ... }`: static local
- function scope 결정 (project_shell_function_scope.md)과 일관

## 신규 kash-specific builtins

| Builtin | 의미 | 모드 |
|---|---|---|
| `die [msg]` | stderr에 msg 출력 + non-zero exit | POSIX-aware+ |
| `assert <expr>` | typeclass 결정 — `[[ ]]` 평가 후 false면 die | POSIX-aware+ |
| `usage [func]` | 명시적 usage 메시지 + exit. doc comment 활용 컨벤션 (세부 미결) | POSIX-aware+ |

(`mode`, `use`, `typeclass`, `instance`는 *keyword*. builtin이 아님.)

## try/catch/okay/finally — first-party utility로 처리 (interface 미확정)

사용자가 first-party utility 카테고리로 처리하기로 결정 — kash 내부에서는 in-process built-in, 외부에서는 symlink 호출 (project_kash_first_party_utils.md의 architecture와 동일).

정확한 interface는 별도 commit (stability rule 적용) — 사용자 확인 후 project_kash_first_party_utils.md 업데이트.

## Builtin override 메커니즘

- `command name` — alias/function 우회 (POSIX)
- `builtin name` — builtin 명시 (ksh93/bash) — 모든 모드 채택
- 명시적 path (`/bin/echo`) — external 강제

## `select` construct

POSIX 정의. 모든 모드 채택.

## 모드별 가용성

| 카테고리 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX special + regular | ✓ | ✓ | ✓ | ✓ | ✓ |
| ksh93 (`print`, `whence`, `compound`, `nameref`, `enum`) | × | × | ✓ | ✓ | ✓ |
| `let` (ksh93/bash 공통) | × | ✓ | ✓ | ✓ | ✓ |
| bash (`mapfile`, `pushd/popd/dirs`, `caller`, `help`, `builtin`) | × | ✓ | × | ✓ | ✓ |
| `local` (typeset alias) | × | ✓ | × (typeset만) | ✓ | ✓ |
| kash 신규 (`die`, `assert`, `usage`) | × | ✓ | × | ✓ | ✓ |
| `read --prompt` | × | ✓ | × | ✓ | ✓ |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** 신규 builtin 추가 시 (1) ksh93/bash 우선 확장 → 충돌 시 long-form, (2) kash-specific은 `die`/`assert` 패턴 따름 (단순 명사 명령), (3) 항상 모드별 가용성 표 명시.
