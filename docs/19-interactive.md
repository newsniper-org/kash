# Interactive Layer (Line Editor + Completion + Bind)

## Line editor — rustyline (포크 가능)

- readline-like Rust 라이브러리
- Highlighter / Hinter / Completer trait API
  - Highlighter → live syntax highlighting (fish 스타일)
  - Hinter → autosuggestion (history 회색 텍스트)
  - Completer → tab completion
- 부족한 부분 (zsh-style multi-line prompt, widget API 등) 은 포크/patch

## Completion — fish-style canonical

```sh
complete -c CMD [-n COND] -a 'arg1 arg2' -d 'desc'
complete -c CMD -l long-form -s s -d '설명'
complete -c CMD -r            # 인자 필수
```

자동 로딩 위치:
- `~/.config/kash/completions/<cmd>.kash`
- `/usr/share/kash/completions/<cmd>.kash`
- `.kashrc.d/completions-*.kash`

### Bash 호환 — **별도 transpiler가 담당**

`complete -F func cmd`, bash-completion 패키지 등은 **bash-completion transpiler** ([12-transpiler.md](12-transpiler.md)) 가 fish-style로 변환. runtime shim 없음.

### zsh compsys — skip

직접 마이그레이션 필요.

## Bind — fish-style canonical

```sh
bind \cf forward-char            # built-in widget
bind \ce 'my-custom-function'    # 사용자 함수 호출
```

function-based — kash의 namespace/typeclass와 자연 결합.

### Bash inputrc 호환 — **별도 transpiler**

`.inputrc`는 **inputrc transpiler** ([12-transpiler.md](12-transpiler.md)) 가 fish-style `bind`로 변환.

### zsh bindkey — skip

## 일관 정책

**모든 bash/zsh 호환은 runtime이 아니라 transpiler가 담당**. kash 자체는 canonical (fish-style) form만. runtime 복잡도 감소 + canonical mental model 단일.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `complete` (fish-style) | × | ✓ | × | ✓ | ✓ |
| `bind` (fish-style) | × | ✓ | × | ✓ | ✓ |
| Live syntax highlight | interactive only | | | | |
| Autosuggestion | interactive only | | | | |

## 미결

- Completion auto-loading 경로 우선순위
- `complete -n COND` 정확한 매칭 의미
- `bind` key sequence 표기 표준 (rustyline KeyCode 매핑)
- Multi-line prompt 시점
- Abbreviation (fish `abbr`) 도입 여부
