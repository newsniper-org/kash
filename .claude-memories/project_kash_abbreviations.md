---
name: kash — fish-style abbreviations (committed)
description: abbr builtin — visible inline expansion (vs alias silent), fish-style subcommand, all modes available with interactive-only expansion
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash abbreviation 시스템. (관련: project_kash_interactive.md, project_kash_config.md)

## 개념

alias와 다름:
- **alias**: 실행 시점 silent 변환. line buffer에 안 보임.
- **abbr**: 입력 시점 visible 확장. 사용자가 trigger key 입력 시 line buffer에 실제 확장 형태로 나타남 — 학습/공유/debugging 친화적.

## Interface — fish-style subcommand canonical

```sh
abbr add NAME EXPANSION                   # 추가
abbr list                                  # 전체 목록
abbr show NAME                             # 단일 조회
abbr remove NAME                           # 제거
abbr rename OLD NEW                        # 이름 변경
abbr clear                                 # 전체 제거
abbr export > backup.kash                  # script form 출력 (재실행 가능)
abbr import backup.kash                    # 불러오기
```

bash-style 플래그 form 없음 — subcommand canonical (history 결정과 일관).

## Trigger 정책

확장 trigger:
- **Default: space, enter**
- 확장 후 사용자가 editing 가능 (visible)
- `--trigger=` customize는 v2 검토

## Position 정책

- **v1: command position only** — line의 첫 단어만 확장
- General position (모든 단어) 은 v2 검토

## Storage

- 위치: `$XDG_CONFIG_HOME/kash/abbreviations.kash` (default `~/.config/kash/abbreviations.kash`)
- Format: kash script (declarative — `abbr add ...` 줄)
- `.kashrc.d/abbreviations-*.kash` 자동 load
- 세션 간 공유 자동

## Visible expansion vs interactive

- **abbr builtin은 모든 모드에서 가용** (POSIX-strict, ksh93u-strict 포함)
- 단 **visible expansion은 interactive shell에서만 trigger됨**
- Non-interactive 스크립트에서 `abbr add ...` 호출 → no-op 또는 저장만 (실제 expansion 없음, 의미 없음)

POSIX-strict와 ksh93u-strict 정책 일관:
- 둘 다 `abbr` builtin 존재, visible expansion은 interactive shell에서만.
- Script 안에서 호출 가능 (config script 등) — 저장됨, interactive REPL에서 expansion됨.

## alias와 공존

- `alias` POSIX 그대로
- `abbr` 신규 (kash 확장)
- 분리:
  - 짧은 명령 자동화 (script에서도 동작) → alias
  - 학습/공유용 visible 입력 확장 (interactive only) → abbr

## Architecture

- `abbr` builtin (in-process)
- Storage = kash script 파일 (사용자가 vim 등으로 직접 편집 가능)
- Interactive layer (rustyline fork) 와 통합 — input 처리 시 abbr 확장

## Transpiler 매핑

- fish `abbr -a NAME EXPANSION` → kash `abbr add NAME 'EXPANSION'`
- fish `abbr -e NAME` → `abbr remove NAME`
- fish-specific `-g`/`-U` (universal vs session): 무시 또는 warning (kash는 universal default)

## 예시

```sh
abbr add g 'git'
abbr add gst 'git status'
abbr add gco 'git checkout'
abbr add k 'kubectl'

# Interactive 사용:
# 'g' + space → line buffer: 'git '
# 'gst' + space → line buffer: 'git status '

abbr list                  # 전체
abbr show g                # → "git"
abbr remove gst
abbr export > my.kash      # backup
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `abbr` builtin | ✓ (interactive only) | ✓ | ✓ (interactive only) | ✓ | ✓ |
| Visible expansion | interactive only | interactive only | interactive only | interactive only | interactive only |

(`abbr` builtin은 모든 모드에서 호출 가능. visible expansion 자체는 모드 무관 — interactive shell이냐 아니냐만 관련.)

## 미결

- General position abbr (v2)
- Trigger customize (v2)
- Universal vs session-only scope (v2)
