---
name: kash — I/O redirection (committed)
description: redirection operators, here-doc/string, fd manipulation, &>의 ksh93 non-conflict, MULTIOS 거부, 네트워크는 first-party utility로
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash I/O redirection 시스템 확정 사항. (관련: project_shell_modes.md, project_shell_subshell_pipeline.md)

## POSIX 기반 (모든 모드 동일)

`>`, `>>`, `<`, `<>`, `n>file`, `n>&m`, `n<&m`, `n>&-`, `n<&-`, `<<DELIM`, `<<-DELIM`, `<<<string`, `>|` (noclobber override).

## 채택한 확장

### `&>file`, `&>>file` — bash style stdout+stderr 단축
- ksh93에서 `&>`는 단일 operator가 아님 → `cmd & > file` (bg + orphan redirect → syntax error)
- 따라서 `&>`를 새 operator로 추가해도 **ksh93 호환 깨지 않음**. `|&` 같은 의미 충돌 case와 다름.
- POSIX-aware, default에서 채택. ksh93u-strict에서는 ksh93에 없으므로 거부.

### `{varname}>file` — auto-fd assignment
- ksh93 정합 (ksh93에 있음). bash 4.1+도 같음.
- POSIX-aware 이상 모두 채택.

### `<<<string` here-string
- POSIX-2024에 정식 채택 — 모든 모드에서 가용.

### `read -u fd`, `print -u fd`
- ksh93 정합. POSIX-aware 이상 채택.

### Persistent redirect via `exec`
POSIX 그대로. `exec 3>file`, `exec 3>&-`, `exec > log` 등 표준.

## 거부한 항목

### zsh MULTIOS — 거부 (모든 모드)
`>a >b` 가 둘 다에 쓰는 zsh 동작은:
- 극도로 surprising
- bash/ksh93/POSIX 어디에도 없음
- 동일 코드가 셸에 따라 silent 다르게 동작 → 가장 큰 비호환 원인

footgun 제거 정신에 반함. 효과 원하면 `tee` 명시 사용.

### bash의 `/dev/tcp/host/port`, `/dev/udp/host/port` 네트워크 redirect
- 직접 채택 거부 (abstraction breach, OS-specific)
- 대신 **first-party utility CLIs로 제공** (project_kash_first_party_utils.md 참조)
- transpiler가 bash의 `/dev/tcp/...` 패턴을 utility 호출로 매핑

## Here-doc 안의 kash expansion flags

따옴표 없는 here-doc에서 kash의 expansion flag도 자연스럽게 동작:
```sh
cat <<EOF
Items: ${(j:, :)arr}
Sorted: ${(o)items}
EOF
```
`<<'EOF'` (quoted)는 POSIX대로 expansion 없음.

## Order semantics (POSIX 그대로)

```sh
cmd >a 2>&1        # stdout과 stderr 모두 a로
cmd 2>&1 >a        # stderr는 원래 stdout으로, stdout만 a로 (흔한 버그)
```

변경 없음. 순서가 의미 결정.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 전체 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<<<` here-string | ✓ (POSIX-2024) | ✓ | ✓ | ✓ | ✓ |
| `&>`, `&>>` | × | ✓ | × | ✓ | ✓ |
| `{varname}>file` auto-fd | × | ✓ | ✓ | ✓ | ✓ |
| `exec n>file` persistent | ✓ | ✓ | ✓ | ✓ | ✓ |
| `read -u fd`, `print -u fd` | × | ✓ | ✓ | ✓ | ✓ |
| MULTIOS (`>a >b`) | × | × | × | × | × |
| 네트워크 redirect (`/dev/tcp/...`) | × | × | × | × | × (utility 사용) |

## 미결

남은 v2+ 항목:
- `warn-fd-leak` option 도입 — process subst, exec 후 close 안 한 패턴 진단 (v2+)

기타 모두 project_kash_sweep_v1.md에서 해소 — auto-fd는 bash 호환(10+), persistent redirect는 lexical scope 따름.

**How to apply:** I/O 관련 후속 결정 (특히 builtin command set의 redirection-aware 기능들) 위 표를 baseline으로. 네트워크 관련은 첫 번째 first-party utility 카테고리 — 거기 결정과 동기화 유지.
