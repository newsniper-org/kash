# kash — 설계 개요

## 프로젝트

새로운 POSIX-compliant 쉘 언어 **kash**를 설계한다.

- 정규 호출명: `kash`
- 작명 의도: **"Korn Again SHell"** — bash ("Bourne Again SHell")의 형식을 따와 *ksh의 계승* 임을 직접 표명. ksh93u+m 문법 baseline 선택과 의미적으로 일관.
- 심볼릭 링크: `sh` (posix-strict + POSIX CLI), `ksh` (ksh93u-strict + ksh93u+m CLI)
- bash 호환은 transpiler로 제공 ([12-transpiler.md](12-transpiler.md))

## 베이스와 우선순위

- **베이스**: ksh93u+m의 *문법* (소스코드가 아닌 문법만).
- **확장**: 위 베이스 위에 zsh 고유 기능 또는 zsh가 더 앞서나가는 기능들을 port.
- **제약 우선순위 (강한 순)**:
  1. **POSIX-compliance** — 가장 강한 요구사항. POSIX sh 스크립트는 POSIX 의미론대로 동작해야 함.
  2. **ksh93u+m 문법** — 베이스로 채택.
  3. **zsh 기능** — 베이스 위에 layered.

## 핵심 원칙

- **새 설계지 복각이 아니다.** ksh93의 quirks/bugs/bad design을 보존할 필요 없음. 더 나은 쪽으로 결정.
- **Footgun 제거 정신.** 모호한 default보다 명확한 에러 또는 명시적 opt-in 우선.
- **모드 시스템으로 의미론 충돌 격리.** 한 모드에서 같은 문법이 다른 의미를 가지지 않음.

## Cross-cutting 문서

- **[INDEX.md](INDEX.md)** — 알파벳순 topic 색인
- **[PRINCIPLES.md](PRINCIPLES.md)** — 설계 원칙 요약
- **[MODE-MATRIX.md](MODE-MATRIX.md)** — 모든 기능 × 모드 가용성 통합 매트릭스
- **[V2-ROADMAP.md](V2-ROADMAP.md)** — v2+ defer된 모든 항목

## 문서 구성

| 번호 | 주제 | 설명 |
|---|---|---|
| [01](01-modes.md) | Mode system | 5 base modes (POSIX-strict/aware, ksh93u-strict/aware, default) + `-secure` modifier |
| [02](02-mode-syntax.md) | Mode declaration syntax | `mode` 키워드, 3 form, symlink (`sh`/`ksh`) CLI 호환 |
| [03](03-function-scope.md) | Function scope and capture | `f()` dynamic, `function f` static, `function f(a, b)` read-only by-ref capture |
| [04](04-arrays.md) | Array and associative array | 0-based, sparse, nested compound 보존 |
| [05](05-compound-vars.md) | Compound variables | `.`/`[]` 둘 다, strict typing, discipline functions |
| [06](06-expansion-flags.md) | Parameter expansion flags | zsh `${(flags)var}` 전체 + compound 확장 |
| [07](07-glob-pattern.md) | Glob and pattern matching | Extglob/`**`/zsh 확장, qualifier는 `(#q...)` |
| [08](08-typeset.md) | Typeset and user-defined types | ksh93 attribute 전체 + `typeset -T` |
| [09](09-typeclass.md) | Typeclasses | Scala 3 inspired, type inference + assertion |
| [10](10-namespace.md) | Namespace system | ksh93 baseline + `use namespace` import + typeclass instance scoping |
| [11](11-subshell-pipeline.md) | Subshell / pipeline / coprocess | `\|&` = coprocess, pipeline 마지막 모드별, process subst 채택 |
| [12](12-transpiler.md) | Bash→kash transpiler & REPL | bash 4.3+ 스크립트 변환, transpiling REPL |
| [13](13-io-redirection.md) | I/O redirection | POSIX + `&>`/auto-fd 채택, MULTIOS 거부, 네트워크는 utility로 |
| [14](14-first-party-utils.md) | First-party utility CLIs | 네트워크 4종(tcp-connect/-listen, udp-send/-recv), in-process + symlink |
| [15](15-set-options.md) | Set options | POSIX core + ksh/bash 확장, 단일 `set -o`, warn-* 4종, `-secure` lock 항목 |
| [16](16-trap-signal.md) | Trap and signal handling | POSIX trap + pseudo-signals, stacking 신규, `.sh.*` context vars |
| [17](17-job-control.md) | Job control | POSIX + 확장, `wait -f`, leaky-jobs 3-option MX 패밀리 |
| [18](18-builtins.md) | Builtin command set | POSIX + ksh93/bash 확장, `read --prompt`, echo/print 정책, `die/assert/usage` 신규 |
| [19](19-interactive.md) | Interactive layer | rustyline + fish-style canonical completion/bind, bash 호환은 transpiler |
| [20](20-config.md) | Config layout | `.kashrc` + `.kashrc.d/*.kash`, `.kash` 확장자 |
| [21](21-implementation.md) | Implementation | Rust 구현, rustyline line editor |
| [22](22-prompt.md) | Prompt system | fish-style canonical functions (`.kash.prompt` etc.), PS1 compat, bash/zsh escape는 transpiler |
| [23](23-history.md) | History system | JSONL, XDG state, incremental shared, `!` opt-in, fish-style subcommand, default unlimited |
| [24](24-oop-extensions.md) | OOP extensions | Dunder lifecycle 2종만, capability는 typeclass, private/static 키워드 |
| [25](25-quote-arithmetic.md) | Quote handling and arithmetic | `$'...'` 모든 모드, `$"..."` transpiler plugin, primitive numeric type set (int*/uint*/float*/bfloat16/complex*/bcomplex32) |
| [26](26-security-policy.md) | Security policy (Shellshock prevention) | 5원칙 — env 비-함수화, Rust 메모리 안전, 외부 데이터 묵시 eval 금지, function source-only, TLS default ON |
| [27](27-std-typeclasses.md) | Standard typeclass library | `.kash.std` prelude 6종 (Eq/Ord/Showable/Hashable/Iterable/Callable) + built-in 자동 instance + `yield` |
| [28](28-sweep-v1.md) | v1 sweep | 누적 미결 ~75개 일괄 해소 (C 6개 + B 50+ + D defer) |
| [29](29-abbreviations.md) | Abbreviations (fish-style) | abbr builtin, visible expansion, 모든 모드 (interactive only) |
| [30](30-module-resolution.md) | Module resolution convention | Path↔namespace 자동, `KASH_MODULE_PATH`, Slackware-style 수동 (manifest는 frontend) |
