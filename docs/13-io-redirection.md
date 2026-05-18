# I/O Redirection

## POSIX 기반 (모든 모드)

`>`, `>>`, `<`, `<>`, `n>file`, `n>&m`, `n<&m`, `n>&-`, `n<&-`, `<<DELIM`, `<<-DELIM`, `<<<string`, `>|` (noclobber override).

## 채택한 확장

### `&>file`, `&>>file` — bash style stdout+stderr 단축

ksh93에서 `&>`는 단일 operator가 아니라 `& >` (bg + orphan redirect → syntax error). 따라서 `&>`를 새 operator로 추가해도 **ksh93 호환을 깨지 않음**. (`|&`와는 다른 case.)

POSIX-aware, default에서 채택. ksh93u-strict 거부.

### `{varname}>file` — auto-fd assignment

ksh93/bash 정합. POSIX-aware 이상 채택.

### `<<<string` here-string

POSIX-2024 정식 채택. 모든 모드 가용.

### `read -u fd`, `print -u fd`

ksh93 정합. POSIX-aware 이상 채택.

### Persistent redirect via `exec`

POSIX 그대로. `exec 3>file`, `exec 3>&-`, `exec > log`.

## 거부한 항목

### zsh MULTIOS (`>a >b`) — 거부 (모든 모드)

극도로 surprising, 셸 간 silent 차이 유발. footgun 제거 정신에 반함.

효과 원하면 `tee` 명시 사용: `cmd | tee a > b`.

### bash의 `/dev/tcp/host/port`, `/dev/udp/host/port`

직접 채택 거부 (abstraction breach, OS-specific). 대신 **first-party utility CLIs로 제공** ([14-first-party-utils.md](14-first-party-utils.md)).

Transpiler가 bash의 `/dev/tcp/...` 패턴을 utility 호출로 매핑.

## Here-doc 안의 kash expansion flags

```sh
cat <<EOF
Items: ${(j:, :)arr}
Sorted: ${(o)items}
EOF
```

따옴표 없는 here-doc에서 kash expansion flag도 자연스럽게 동작. `<<'EOF'` (quoted)는 POSIX대로 expansion 없음.

## Order semantics (POSIX 그대로)

```sh
cmd >a 2>&1        # stdout, stderr 모두 a로
cmd 2>&1 >a        # stderr는 원래 stdout으로, stdout만 a로 (흔한 버그)
```

순서가 의미 결정. 변경 없음.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 전체 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<<<` here-string | ✓ | ✓ | ✓ | ✓ | ✓ |
| `&>`, `&>>` | × | ✓ | × | ✓ | ✓ |
| `{varname}>file` auto-fd | × | ✓ | ✓ | ✓ | ✓ |
| `exec n>file` persistent | ✓ | ✓ | ✓ | ✓ | ✓ |
| `read -u fd`, `print -u fd` | × | ✓ | ✓ | ✓ | ✓ |
| MULTIOS (`>a >b`) | × | × | × | × | × |
| 네트워크 redirect (`/dev/tcp/...`) | × | × | × | × | × (utility 사용) |

## 미결

- File descriptor leak 패턴의 진단/경고 메커니즘
- Auto-fd `{varname}>file`의 정확한 fd 할당 범위 (bash는 10+부터)
- Persistent redirect의 mode 시스템과의 상호작용
