# Config Layout and File Conventions

## Script 확장자 — `.kash`

- kash script 표준 확장자.
- Shebang: `#!/usr/bin/env kash`
- Shebang-less `.kash` 파일도 kash로 인식.
- Transpiler 출력 기본 이름: `script.bash` → `script.kash`

## 사용자 config

### 주 config
- `~/.kashrc` — 메인 config 파일 (kash script)

### 모듈화 디렉토리
- `~/.kashrc.d/*.kash` — 모든 `.kash` 파일 lexical 순서로 자동 load
- Modern Linux `.d/` 패턴 (vim, systemd, sudo) 정합
- 패키지 매니저 third-party config drop-in 친화적

### Load 순서
1. 시스템 config (예: `/etc/kashrc`, `/etc/kashrc.d/*.kash` — 정확한 경로 미확정)
2. `~/.kashrc`
3. `~/.kashrc.d/*.kash` (lexical order)

(`00-defaults.kash`, `10-completions.kash`, `90-local.kash` 같은 prefix로 순서 통제)

### Interactive vs non-interactive
- Interactive shell: 전체 load
- Non-interactive script: skip (POSIX 관례 — `KASH_ENV` 같은 별도 메커니즘은 미정)

## Completion 자동 로딩

- `~/.config/kash/completions/<cmd>.kash`
- `/usr/share/kash/completions/<cmd>.kash`
- `.kashrc.d/completions-*.kash`

## 미결

- 시스템 config 정확한 경로
- Non-interactive env-var 기반 config load 메커니즘
- `~/.kash_history` 등 state 파일 위치 (XDG Base Dir 준수?)
- 패키지 manifest / locking 메커니즘
