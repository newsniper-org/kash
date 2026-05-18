# Abbreviations (fish-style)

## 개념

| | alias | abbr |
|---|---|---|
| 변환 시점 | 실행 시 (silent) | 입력 시 (visible) |
| Line buffer | 변하지 않음 | 즉시 확장 |
| 용도 | 짧은 명령 자동화 | 학습/공유/debugging |
| Script 동작 | ✓ | × (interactive only) |

## Interface — fish-style subcommand canonical

```sh
abbr add NAME EXPANSION
abbr list
abbr show NAME
abbr remove NAME
abbr rename OLD NEW
abbr clear
abbr export > backup.kash
abbr import backup.kash
```

bash-style 플래그 form 없음.

## Trigger

- Default: **space, enter**
- 확장 후 사용자가 editing 가능
- `--trigger=` customize v2+

## Position

- v1: **command position only** (첫 단어)
- General position v2+

## Storage

- 위치: `$XDG_CONFIG_HOME/kash/abbreviations.kash`
- Format: kash script (`abbr add ...` 선언)
- `.kashrc.d/abbreviations-*.kash` 자동 load
- 세션 간 공유 자동

## Visible expansion vs interactive

- `abbr` builtin은 **모든 모드에서 가용** (POSIX-strict, ksh93u-strict 포함)
- Visible expansion은 **interactive shell에서만 trigger**
- Non-interactive 스크립트에서 호출 → 저장만, 실제 확장 없음

POSIX-strict / ksh93u-strict 동일 정책: builtin 존재, expansion은 interactive only.

## alias와 공존

- alias: POSIX 그대로, script 호환
- abbr: kash 확장, interactive only

## Transpiler 매핑

- fish `abbr -a NAME EXPANSION` → kash `abbr add NAME 'EXPANSION'`
- fish `abbr -e NAME` → `abbr remove NAME`
- fish `-g`/`-U` flag: 무시/warning (kash는 universal default)

## 예시

```sh
abbr add g 'git'
abbr add gst 'git status'

# Interactive: 'g' + space → line buffer: 'git '
# Interactive: 'gst' + space → 'git status '

abbr list
abbr show g
abbr remove gst
abbr export > my.kash
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `abbr` builtin | ✓ (interactive only) | ✓ | ✓ (interactive only) | ✓ | ✓ |
| Visible expansion | interactive only (모든 모드 공통) | | | | |

## 미결

- General position abbr (v2)
- Trigger customize (v2)
- Universal vs session-only scope (v2)
