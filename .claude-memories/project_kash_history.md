---
name: kash — history system (committed)
description: JSONL format, XDG state location, incremental shared append, modern UX defaults, fish-style subcommand canonical
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash history 시스템 확정 사항. (관련: project_kash_interactive.md, project_kash_config.md, project_kash_prompt.md)

## Format — JSONL

한 줄당 한 JSON object. 필수 필드:

```json
{"ts":"2026-05-17T13:30:00Z","cwd":"/home/ybi/kash","cmd":"git status","exit":0,"dur_ms":42,"session":"abc123"}
```

이유:
- append atomicity (한 줄)
- machine-readable — `json-get` utility (예정)로 검색/분석
- 필드 추가만으로 backward compat (unknown 필드 무시)

대안 (SQLite 등)은 v2 검토.

## Location — XDG 준수

```
$XDG_STATE_HOME/kash/history     # 우선
~/.local/state/kash/history       # XDG_STATE_HOME 없을 시 default
```

`HISTFILE` 환경변수 set되면 override (bash 호환).

History는 *state* 카테고리 — `$XDG_STATE_HOME` 이 정확 (fish는 `_DATA_`인데 그건 XDG 정착 전 결정).

## Session 간 공유 — incremental append + shared 기본

- 매 명령마다 즉시 file에 append
- 다른 세션 새 entry를 next prompt에 읽음
- 동시 쓰기는 file lock (JSONL line-atomicity 활용)
- bash의 "exit 시 dump" 모델 거부 (footgun — 세션 충돌 시 loss)

## Ignore policy — 3가지 옵션 + pattern array

| 옵션 | 의미 | default |
|---|---|---|
| `hist-ignore-dups` | 직전 명령과 동일하면 skip | default: on |
| `hist-ignore-space` | 선행 공백 명령 skip | default: on |
| `hist-ignore-erasedups` | 동일 명령 과거 entry 제거 후 저장 | default: off |

`.kash.history.ignore_patterns` array (glob 패턴) — 매칭 명령 저장 안 함. (`HISTIGNORE`의 kash form)

`-secure`는 history에 lock 안 함 — 사용자 명시적 통제.

모드별:
- POSIX-strict/ksh93u-strict: ignore 옵션 비활성
- POSIX-aware/ksh93u-aware: 가능, default off
- default: `hist-ignore-dups + hist-ignore-space` on

## Size limits

- `HISTSIZE` (in-memory entries) — bash/zsh 변수 호환
- `HISTFILESIZE` (file entries) — bash 호환 (zsh `SAVEHIST` 별칭)
- **default 무한대** (modern storage 기준 — 사용자 명시 truncation 원할 때만 설정)

## History expansion (`!!`, `!n`, `^old^new`) — default OFF, opt-in

```sh
set -o histexpand            # opt-in
```

이유:
- Quoting 함정 (`!` interactive 환경에서)
- 모던 셸 (fish) 폐기
- Ctrl-R / 화살표가 더 discoverable

POSIX-aware/ksh93u-aware/default: 기본 off, opt-in 가능.
POSIX-strict/ksh93u-strict: 비활성.

Script 안의 `!` 사용은 거의 없음 — transpiler가 만나면 명시적 `fc`로 변환.

## `history` builtin — fish-style subcommand canonical

```sh
history                              # 최근 entry 출력
history -n 50                        # 50개
history search 'pattern'             # 패턴 매칭
history search --cwd /some/dir       # cwd 필터
history search --since '1h'          # 시간 필터
history search --exit 0              # 성공한 것만
history delete --exact 'rm -rf /'    # 정확 일치 제거
history delete --pattern 'rm *'      # 패턴
history clear                        # 전체
history merge                        # 다른 세션 변경분 reload (자동도 됨)
history export --format jsonl > backup.jsonl
history import backup.jsonl
```

bash의 `history -c`, `history -d N` 등은 transpiler가 매핑.

POSIX `fc` builtin은 그대로 유지 (호환).

## Interactive search

- rustyline reverse-i-search (Ctrl-R) canonical
- fuzzy search는 future enhancement (별도 first-party utility `history-search` 가능성)

## Metadata 노출 (`.kash.history.*`)

```kash
echo "Last command exit: ${.kash.history.last.exit}"
echo "Took: ${.kash.history.last.dur_ms} ms"
```

- `.kash.history.last` — 가장 최근 명령 entry (compound)
  - `.cmd`, `.cwd`, `.ts`, `.exit`, `.dur_ms`, `.session`
- `.kash.history.search_result` — 마지막 search 결과 (array of compound)
- prompt 함수에서 활용 가능 (last duration 표시 등)

## Sensitive command 처리

- `hist-ignore-space` (선행 공백)
- `set -o nohist` (잠정 비활성 토글)
- 명시적 `history delete` 사용

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| History 자체 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `fc` builtin | ✓ | ✓ | ✓ | ✓ | ✓ |
| `history` builtin (subcommand) | × | ✓ | × | ✓ | ✓ |
| Incremental sharing | (impl) | ✓ | (impl) | ✓ | ✓ |
| Timestamps/cwd/exit per entry | n/a | ✓ | n/a | ✓ | ✓ |
| `hist-ignore-dups/-space` | × | ✓ | × | ✓ | ✓ (default on) |
| `!` history expansion | × | ✓ (opt-in) | × | ✓ (opt-in) | ✓ (opt-in) |
| `.kash.history.*` introspection | × | ✓ | × | ✓ | ✓ |

## 미결

남은 v2+ 항목:
- Fuzzy search 알고리즘 (fzf-like) 도입 시 의존성 결정 — v2+
- History → first-party utility로 일부 분리 (`history-search` 등) — v2+

기타 모두 project_kash_sweep_v1.md에서 해소 — `nohist` 이름, `flock(2)`, multi-line은 escaped newline JSON.

**How to apply:** history 관련 후속 결정 (encryption, sync, scoped history 등 v2+) 시 JSONL format을 baseline으로. `.kash.history.*` namespace 일관 유지.
