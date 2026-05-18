# History System

## Format — JSONL

한 줄당 한 JSON object. 필수 필드:

```json
{"ts":"2026-05-17T13:30:00Z","cwd":"/home/ybi/kash","cmd":"git status","exit":0,"dur_ms":42,"session":"abc123"}
```

machine-readable, append atomic, 필드 추가만으로 backward compat.

## Location — XDG 준수

- `$XDG_STATE_HOME/kash/history`
- 없으면 `~/.local/state/kash/history`
- `HISTFILE` env var override (bash 호환)

History는 *state* 카테고리 — `$XDG_STATE_HOME`이 정확.

## Session 공유 — incremental + shared (기본)

- 매 명령 즉시 append
- 다른 세션 new entry를 next prompt에 reload
- File lock (line atomicity 활용)

bash의 "exit 시 dump" 모델 거부 (세션 충돌 시 loss).

## Ignore policy

| 옵션 | 의미 | default |
|---|---|---|
| `hist-ignore-dups` | 직전 명령과 동일 skip | on |
| `hist-ignore-space` | 선행 공백 명령 skip | on |
| `hist-ignore-erasedups` | 동일 명령 과거 entry 제거 | off |

`.kash.history.ignore_patterns` array (glob).

`-secure`는 history lock 안 함.

## Size limits

- `HISTSIZE` (in-memory), `HISTFILESIZE` (file)
- **default 무한대** (modern storage)
- 사용자 명시 시만 truncation

## History expansion (`!!` 등) — default OFF, opt-in

```sh
set -o histexpand
```

fish 노선 — quoting 함정 + Ctrl-R가 더 discoverable.

## `history` builtin — fish-style subcommand

```sh
history
history -n 50
history search 'pattern'
history search --cwd /some/dir
history search --since '1h'
history search --exit 0
history delete --exact 'rm -rf /'
history delete --pattern 'rm *'
history clear
history merge
history export --format jsonl > backup.jsonl
history import backup.jsonl
```

bash flag form (`history -c`, `-d`)은 transpiler 매핑.

POSIX `fc` 유지.

## Interactive search

rustyline reverse-i-search (Ctrl-R) canonical. Fuzzy는 future.

## Metadata 노출

```kash
${.kash.history.last.cmd}
${.kash.history.last.exit}
${.kash.history.last.dur_ms}
${.kash.history.last.cwd}
${.kash.history.last.ts}
```

prompt 함수에서 활용 가능.

## Sensitive command

- `hist-ignore-space`
- `set -o nohist` (잠정)
- `history delete`

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| History 자체 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `fc` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `history` subcommand | × | ✓ | × | ✓ | ✓ |
| Incremental sharing | (impl) | ✓ | (impl) | ✓ | ✓ |
| Timestamps/cwd/exit | n/a | ✓ | n/a | ✓ | ✓ |
| `hist-ignore-*` | × | ✓ | × | ✓ | ✓ (default on) |
| `!` expansion | × | ✓ (opt-in) | × | ✓ (opt-in) | ✓ (opt-in) |
| `.kash.history.*` | × | ✓ | × | ✓ | ✓ |

## 미결

- `nohist` 옵션 정확한 이름
- File lock 메커니즘 (flock vs OS native)
- Multi-line command JSON 표현
- Fuzzy search 알고리즘
- History → first-party utility 분리 가능성
