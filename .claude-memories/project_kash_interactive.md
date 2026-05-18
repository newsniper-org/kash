---
name: kash — interactive layer (line editor + completion + bind, committed)
description: rustyline 기반 line editor, fish-style canonical completion + bind, bash/zsh 호환은 transpiler/REPL이 처리
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash 인터랙티브 layer (line editor, completion, key binding) 확정 사항. (관련: project_kash_implementation.md, project_shell_transpiler.md)

## Line editor — rustyline (포크 가능)

- rustyline의 readline-like API 활용
- Highlighter / Hinter / Completer trait를 fish-style UX에 매핑
  - Highlighter → live syntax highlighting (fish의 빨강/초록)
  - Hinter → autosuggestion (history 기반 회색 텍스트)
  - Completer → tab completion
- 추가 기능 필요 시 포크 (multi-line prompt, custom widget API 등)

## Completion — fish-style canonical, bash 호환은 *transpiler가 처리*

### Canonical: fish-style declarative
```sh
complete -c CMD [-n COND] -a 'arg1 arg2' -d 'desc'
complete -c CMD -l long-form -s s -d '설명'
complete -c CMD -r            # 인자 필수
```

자동 로딩 위치 (관례):
- `~/.config/kash/completions/<cmd>.kash`
- (시스템) `/usr/share/kash/completions/<cmd>.kash`
- `.kashrc.d/completions-*.kash` 도 자동 load

### Bash 호환 — **runtime shim 없음, 전적으로 *별도 transpiler*가 담당**

⚠️ **bash-completion / inputrc는 *script transpiler와 별개 transpiler*** (입력 문법 자체가 다름):
- **bash-completion transpiler**: `complete -F func cmd`, `compgen`, `compopt`, bash-completion 패키지 (`/usr/share/bash-completion/completions/*`) → fish-style `complete -c ...`
- **inputrc transpiler**: `.inputrc` (readline key binding syntax) → fish-style `bind`
- **script transpiler**: 일반 bash 스크립트 (.bashrc 등)

세부는 project_shell_transpiler.md 참조.

transpiling REPL은 입력 종류 자동 감지 또는 명시적 모드 전환으로 적절한 transpiler 호출.

kash 자체는 fish form만 인식 — runtime 복잡도 감소.

### zsh compsys — **skip**
표현력 높지만 복잡도 대비 가치 낮음. zsh 사용자가 옮겨올 때 직접 fish-style로 마이그레이션 (자동 변환 미제공 — 의미 매핑 너무 복잡).

## Bind (key bindings) — fish-style canonical, bash inputrc는 *transpiler가 처리*

### Canonical: fish-style function-based
```sh
bind \cf forward-char            # Ctrl+F → forward-char (built-in widget)
bind \ce 'my-custom-function'    # Ctrl+E → 사용자 정의 함수 호출
```

- function-based — kash의 namespace/typeclass와 자연스럽게 결합
- rustyline KeyEvent에 매핑

### Bash inputrc 호환 — transpiler 담당
- `.inputrc` 형식 (`"\C-x\C-r": re-read-init-file` 등) → kash form 변환
- `bind '"\C-x": ...'`, `bind -x` 등 — 변환

### zsh bindkey — skip 또는 transpiler에서만
zsh의 `bindkey -M mainmap ...` 같은 keymap concept은 fish의 단순 model로 표현 어려움. skip.

## 일관 정책

**모든 bash/zsh 호환은 runtime이 아니라 transpiler/transpiling REPL이 담당.** kash 자체는 canonical (fish-style) form만 가짐. 이 정책은 completion/bind뿐 아니라 향후 bash-only 인터페이스 일반에 적용.

- Runtime 복잡도 감소
- canonical form이 명확 — "이게 kash의 방식" 단일 mental model
- bash 사용자는 마이그레이션 시 transpiler 한 번 돌리면 됨

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `complete` (fish-style) | × | ✓ | × | ✓ | ✓ |
| `bind` (fish-style) | × | ✓ | × | ✓ | ✓ |
| Live syntax highlighting | (interactive only) | (interactive only) | (interactive only) | (interactive only) | (interactive only) |
| Autosuggestion | (interactive only) | (interactive only) | (interactive only) | (interactive only) | (interactive only) |

(interactive only = 비-interactive 실행과 무관, 모드와 직교)

## 미결

남은 항목 (impl detail / v2+):
- `complete`의 conditional (`-n COND`) 정확한 매칭 의미 — rustyline 통합 시 impl
- `bind`에서 key sequence 표기 표준 — rustyline KeyCode 매핑 impl
- Multi-line prompt — rustyline 18.0.0 fork에서 patch (v2+)
- Mode line (terminal status line) — v2+
- Abbreviation (`abbr` fish-style) — 별도 결정 round

Completion 경로 우선순위는 project_kash_sweep_v1.md에서 해소 (`$XDG_CONFIG_HOME/kash/completions/` > `/usr/share/kash/completions/`).

**How to apply:** 인터랙티브 layer 후속 결정 (prompt, history, abbreviation 등) 시 fish-style canonical + transpiler 호환 패턴 유지.
