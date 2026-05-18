---
name: kash — config layout and file conventions (committed)
description: .kashrc + .kashrc.d/*.kash, .kash 확장자 표준
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 사용자 설정 / 파일 컨벤션.

## Script 확장자 — `.kash`

- kash script 표준 확장자.
- Shebang: `#!/usr/bin/env kash`
- Shebang-less `.kash` 파일도 kash로 인식 (mode line 검사).
- Transpiler 출력: `script.bash` → `script.kash` 자동 변환 기본 이름.

## 사용자 config

### 주 config
- `~/.kashrc` — 메인 config 파일 (kash script)

### 모듈화 디렉토리
- `~/.kashrc.d/*.kash` — 모든 `.kash` 파일 lexical 순서로 자동 load
- Modern Linux의 `.d/` 패턴 (vim, systemd, sudo 등) 정합
- 패키지 매니저가 third-party config drop-in 하기 좋음 (`apt install kash-completion` → `.kashrc.d/` 추가)

### Load 순서
1. 시스템 config (예: `/etc/kashrc`, `/etc/kashrc.d/*.kash` — 정확한 경로 미확정)
2. `~/.kashrc`
3. `~/.kashrc.d/*.kash` (lexical order)

(흔히 사용되는 패턴: `00-defaults.kash`, `10-completions.kash`, `90-local.kash` 같은 prefix로 순서 통제)

### Interactive vs non-interactive
- Interactive shell: 전체 config load
- Non-interactive script: config skip (POSIX 셸 관례 — `BASH_ENV` 같은 별도 메커니즘은 미정)

## Completion 자동 로딩 경로 (관례)

- `~/.config/kash/completions/<cmd>.kash`
- `/usr/share/kash/completions/<cmd>.kash`
- `.kashrc.d/completions-*.kash` 도 가능

(관련: project_kash_interactive.md)

## 미결

남은 v2+ 항목:
- 패키지 manifest / locking 메커니즘 (모듈 시스템 v2+)

기타 모두 project_kash_sweep_v1.md에서 해소 — 시스템 config 경로는 `/etc/kashrc` + `/etc/kashrc.d/*.kash`, `KASH_ENV` 도입, state file은 `$XDG_STATE_HOME/kash/`.

**How to apply:** 향후 추가될 config/state file은 `.kash` 또는 `.config/kash/` 하위 일관 사용. XDG Base Dir 준수 가능하면 그쪽 권장 (modern Linux 환경).
