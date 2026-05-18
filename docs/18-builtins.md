# Builtin Command Set

## POSIX baseline (모든 모드)

### Special builtins
`break`, `:`, `continue`, `.` (source), `eval`, `exec`, `exit`, `export`, `readonly`, `return`, `set`, `shift`, `trap`, `unset`.

### Regular builtins
`cd`, `echo`, `false`, `printf`, `pwd`, `read`, `test`/`[`, `true`, `umask`, `wait`, `getopts`, `command`, `times`, `alias`/`unalias`, `hash`, `type`, `select`.

## 채택할 ksh93 확장

| Builtin | 의미 |
|---|---|
| `print` | 일관된 출력 (canonical) |
| `print -r/-n/-u/-p` | raw/no-newline/fd/coproc |
| `whence [-vap]` | command resolution |
| `let` | 산술 평가 |
| `compound`, `nameref` | `typeset -C/-n` alias |
| `redirect` | exec에 redirection 적용 |
| `time` | timing (keyword + builtin) |
| `enum` | enumerated type |

## 채택할 bash 확장

| Builtin | 의미 |
|---|---|
| `mapfile` / `readarray` | 파일/stdin → 배열 |
| `pushd` / `popd` / `dirs` | 디렉토리 스택 |
| `caller` | 호출 stack info |
| `help` | builtin 도움말 |
| `builtin` | 특정 builtin 강제 실행 |

## 거부

- `enable -n` — 위험
- `complete`/`compgen` — line editor layer로
- `bind` — line editor layer로

## 충돌 해소

### `read -p` (ksh93 coproc vs bash prompt)
- **ksh93 의미 유지**: `read -p` = coproc 읽기
- **Prompt은 long-form**: `read --prompt "string" var`
- Transpiler가 bash form → kash long form 매핑

### `echo` 정책
- POSIX-strict: POSIX 의미
- 그 외: implementation-defined / ksh93 의미
- default: `warn-echo-portability` 옵션 권장
- 진지한 출력은 `printf` 권장

### `local` = `typeset` alias
- POSIX form `f() { local x; }`: dynamic
- ksh form `function f { local x; }`: static
- [function scope](03-function-scope.md) 결정과 일관

## 신규 kash builtins

| Builtin | 의미 |
|---|---|
| `die [msg]` | stderr 출력 + non-zero exit |
| `assert <expr>` | `[[ ]]` 평가 후 false면 die |
| `usage [func]` | usage 메시지 + exit (doc comment 컨벤션, 세부 미결) |

(`mode`, `use`, `typeclass`, `instance`는 keyword.)

## try/catch/okay/finally

First-party utility 카테고리로 처리. kash 내부 in-process builtin, 외부 symlink ([14-first-party-utils.md](14-first-party-utils.md) architecture). Interface 미확정 (stability rule 적용).

## Override 메커니즘

- `command name` — alias/function 우회
- `builtin name` — builtin 명시
- 명시적 path — external 강제

## 모드별 가용성

| 카테고리 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX special + regular | ✓ | ✓ | ✓ | ✓ | ✓ |
| ksh93 (`print`, `whence`, `compound`, `nameref`, `enum`) | × | × | ✓ | ✓ | ✓ |
| `let` | × | ✓ | ✓ | ✓ | ✓ |
| bash (`mapfile`, `pushd/popd/dirs`, `caller`, `help`, `builtin`) | × | ✓ | × | ✓ | ✓ |
| `local` | × | ✓ | × | ✓ | ✓ |
| kash 신규 (`die`, `assert`, `usage`) | × | ✓ | × | ✓ | ✓ |
| `read --prompt` | × | ✓ | × | ✓ | ✓ |

## 미결

- `warn-echo-portability` 옵션 추가
- `usage` doc comment 컨벤션
- `select` 응답 prompt 형식
- `mapfile` 옵션 set 확정
- `caller` 출력 형식 (bash 호환 vs kash 새 form)
