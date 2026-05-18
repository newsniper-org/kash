# Mode Availability Matrix

모든 기능의 모드별 가용성 통합 매트릭스. 각 기능의 세부는 해당 문서 참조.

범례:
- ✓ : 가용
- × : 비활성 (parse/eval에서 거부)
- — : 의미 없음 (mode와 직교)
- 괄호 : 특수 동작

## Base modes

| Base | 정의 |
|---|---|
| POSIX-strict | POSIX 정의 기능만 |
| POSIX-aware | 모든 확장 가용, corner case POSIX 따름 |
| ksh93u-strict | ksh93u+m 매뉴얼 기능만 |
| ksh93u-aware | 모든 기능 가용, 충돌 시 ksh93 우선 |
| default | 전체 기능, 충돌 시 새 셸의 더 안전한 default |

## Modifier

| Modifier | 의미 |
|---|---|
| `-secure` | footgun-elimination 프로파일. monotonic. |

## 언어 — Function scope & capture ([03](03-function-scope.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default | -secure |
|---|---|---|---|---|---|---|
| `f() { }` (POSIX dynamic) | ✓ | ✓ | ✓ | ✓ | ✓ | inherit |
| `function f { }` static | × | ✓ | ✓ | ✓ | ✓ | inherit |
| `function f() { }` (= function f) | × | ✓ | ✓ | ✓ | ✓ | inherit |
| `function f(a, b)` read-only capture | × | ✓ | × | ✓ | ✓ | inherit |
| `local` (typeset alias) | × | ✓ | × (typeset만) | ✓ | ✓ | inherit |

## 데이터 — Arrays ([04](04-arrays.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| 배열 자체 | × | ✓ | ✓ | ✓ | ✓ |
| ksh form (`arr=(a b c)`, `${arr[i]}`) | × | ✓ | ✓ | ✓ | ✓ |
| Nested compound | × | ✓ | ✓ | ✓ | ✓ |
| Negative indexing (`${arr[-1]}`) | × | ✓ | × | ✓ | ✓ |
| zsh-style slicing (`${arr[@]:s..e}`) | × | × | × | ✓ | ✓ |
| Implicit creation | × | ✓ | ✓ | ✓ | ✓ |
| Indexed에 string key | × | ✓ (산술 0) | × | ✓ (산술 0) | × (error) |

## 데이터 — Compound vars ([05](05-compound-vars.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| Compound var | × | ✓ | ✓ | ✓ | ✓ |
| `.` member access | × | ✓ | ✓ | ✓ | ✓ |
| Compound `[]` 접근 | × | ✓ | ✓ | ✓ | ✓ |
| Assoc에 `.` 접근 | × | × | ✓ | ✓ | × (strict typing) |
| Compound에 임의 string key | × | × | ✓ | ✓ | × |
| Discipline functions | × | ✓ | ✓ | ✓ | ✓ |
| `.sh.*` 보존 | × | ✓ | ✓ | ✓ | ✓ |
| 멤버 type 암묵 변환 | × | × | × | × | × (모든 모드 금지) |

## 데이터 — Expansion flags ([06](06-expansion-flags.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default | -secure |
|---|---|---|---|---|---|---|
| `${(flag)var}` 표기 | × | ✓ | × | ✓ | ✓ | base 따름 |
| ksh `${!var[@]}` 등 | × | ✓ | ✓ | ✓ | ✓ | base 따름 |
| Compound 확장 `(k)/(v)/(t)` | × | ✓ | × | ✓ | ✓ | base 따름 |
| `(e)` re-eval | × | ✓ (+ warning) | × | ✓ (+ warning) | ✓ (+ warning) | **금지** |

## 데이터 — Glob/Pattern ([07](07-glob-pattern.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 (`*`, `?`, `[]`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Extglob | × | ✓ | ✓ | ✓ | ✓ |
| `**` recursive | × | ✓ | ✓ | ✓ | ✓ |
| zsh 확장 (`^`, `~`, `<a-b>`, `#`) | × | ✓ | × | ✓ | ✓ |
| Glob qualifiers `(#q...)` | × | ✓ | × | ✓ | ✓ |
| Brace step `{1..10..2}` | × | ✓ | × | ✓ | ✓ |
| `case` `;&`, `;;&` | × | ✓ | × | ✓ | ✓ |
| `=~` ERE | × | ✓ | ✓ | ✓ | ✓ |

### Null glob ([07](07-glob-pattern.md))

| 모드 | 동작 |
|---|---|
| POSIX-strict | unchanged |
| POSIX-strict-secure | fail |
| POSIX-aware | unchanged |
| POSIX-aware-secure | fail |
| ksh93u-strict | unchanged |
| ksh93u-strict-secure | fail |
| ksh93u-aware | unchanged |
| ksh93u-aware-secure | fail |
| default | **fail** |
| default-secure | fail |

## Quote/Arithmetic ([25](25-quote-arithmetic.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `'..'`, `".."`, `\X` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `$'...'` ANSI-C | ✓ (POSIX-2024) | ✓ | ✓ | ✓ | ✓ |
| `\u{HEX}` (ksh93 form) | (POSIX 미정의) | ✓ | ✓ | ✓ | ✓ |
| `$"..."` (gettext) | × (transpiler) | × | × | × | × |
| `$((expr))` 정수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `((expr))` command | × | ✓ | ✓ | ✓ | ✓ |
| `let` builtin | × | ✓ | ✓ | ✓ | ✓ |
| `int64` 기본 | × | ✓ | ✓ | ✓ | ✓ |
| Wide integer (int8/16/32/128, uint*) | × | × | × | ✓ | ✓ |
| `float64` | × | × | ✓ | ✓ | ✓ |
| Specialty float (`float16/bfloat16/128`) | × | × | × | ✓ | ✓ |
| Complex/bcomplex | × | × | × | ✓ | ✓ |
| Math library | × | × | ✓ | ✓ | ✓ |
| Imaginary unit `1+2i` | × | × | × | ✓ | ✓ |

## 데이터 — Typeset / Types ([08](08-typeset.md), [24](24-oop-extensions.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `typeset` 자체 | × | ✓ | ✓ | ✓ | ✓ |
| POSIX 인접 attrs (`-a/-A/-i/-r/-x`) | × (readonly/export만) | ✓ | ✓ | ✓ | ✓ |
| `-C` compound | × | ✓ | ✓ | ✓ | ✓ |
| `-T` type definition | × | ✓ | ✓ | ✓ | ✓ |
| `-E`/`-F` floats | × | ✓ | ✓ | ✓ | ✓ |
| `-L`/`-R`/`-Z` justify | × | ✓ | ✓ | ✓ | ✓ |
| `-M` map | × | ✓ | ✓ | ✓ | ✓ |
| `-U` (zsh 채택) | × | ✓ | × | ✓ | ✓ |
| Dunder (`__init`, `__del`) | × | ✓ | × | ✓ | ✓ |
| `private` keyword | × | ✓ | × | ✓ | ✓ |
| `static` keyword | × | ✓ | × | ✓ | ✓ |
| `MyType x(args)` constructor | × | ✓ | × | ✓ | ✓ |

## 데이터 — Typeclasses ([09](09-typeclass.md), [27](27-std-typeclasses.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `typeclass` 선언 | × | ✓ | × | ✓ | ✓ |
| `instance` 선언 | × | ✓ | × | ✓ | ✓ |
| Method dispatch | × | ✓ | × | ✓ | ✓ |
| `requires` (inheritance) | × | ✓ | × | ✓ | ✓ |
| `[[ -is ]]`, `[[ -satisfies ]]` | × | ✓ | × | ✓ | ✓ |
| `assert` builtin | × | ✓ | × | ✓ | ✓ |
| `.kash.std` prelude (auto-import) | × | ✓ | × | ✓ | ✓ |
| `Eq/Ord/Showable/Hashable/Iterable/Callable` | × | ✓ | × | ✓ | ✓ |
| Built-in type 자동 instances | × | ✓ | × | ✓ | ✓ |
| `yield` 키워드 | × | ✓ | × | ✓ | ✓ |
| `for x in iterable_var` lazy iter | × | ✓ | × | ✓ | ✓ |

## 모듈/Namespace ([10](10-namespace.md), [30](30-module-resolution.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `namespace` 선언 | × | ✓ | ✓ | ✓ | ✓ |
| Nested / Reopening | × | ✓ | ✓ | ✓ | ✓ |
| `use namespace` (import) | × | ✓ | × | ✓ | ✓ |
| 부분 import (`use .ns.x`) | × | ✓ | × | ✓ | ✓ |
| Typeclass instance namespace scoping | × | ✓ | × | ✓ | ✓ |
| Path → namespace 자동 (module resolution) | × | ✓ | × | ✓ | ✓ |
| `KASH_MODULE_PATH` | × | ✓ | × | ✓ | ✓ |

## 실행 — Subshell/Pipeline ([11](11-subshell-pipeline.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `(cmd)`, `$(cmd)`, backticks | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<(cmd)`, `>(cmd)` process subst | × | ✓ | ✓ | ✓ | ✓ |
| `\|&` coprocess | × | ✓ | ✓ | ✓ | ✓ |
| `coproc name cmd` (multi) | × | ✓ | × | × | ✓ |
| Pipeline 마지막 = 현재 shell | × | × | ✓ | ✓ | ✓ |
| `wait -n/-p/-f` | × | ✓ | × | ✓ | ✓ |
| Backtick deprecation warning | × | × | × | × | ✓ (옵션) |
| Subshell semantics | POSIX (모든 모드 동일 — ksh93 최적화 미적용) | | | | |

## 실행 — I/O Redirection ([13](13-io-redirection.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `<<<` here-string | ✓ | ✓ | ✓ | ✓ | ✓ |
| `&>`, `&>>` | × | ✓ | × | ✓ | ✓ |
| `{varname}>file` auto-fd | × | ✓ | ✓ | ✓ | ✓ |
| `exec n>file` persistent | ✓ | ✓ | ✓ | ✓ | ✓ |
| `read -u fd`, `print -u fd` | × | ✓ | ✓ | ✓ | ✓ |
| MULTIOS (`>a >b`) | × | × | × | × | × |
| 네트워크 redirect (`/dev/tcp/...`) | × | × | × | × | × (utility 사용) |

## 실행 — Set options ([15](15-set-options.md))

| 옵션 | strict | aware | ksh93u-strict | ksh93u-aware | default | -secure |
|---|---|---|---|---|---|---|
| `errexit` | off | off | off | off | off | **on (lock)** |
| `pipefail` | off | off | off | off | off | **on (lock)** |
| `nounset` | off | off | off | off | off | **on (lock)** |
| `noclobber` | off | off | off | off | off | **on (lock)** |
| `warn-backticks` | n/a | off | off | off | off | **on (lock)** |
| `warn-unsafe-eval` | n/a | off | n/a | off | off | **on (lock)** |
| `warn-implicit-array` | n/a | off | n/a | off | off | **on (lock)** |
| `warn-leaky-glob` | n/a | off | off | off | off | **on (lock)** |
| `warn-echo-portability` | n/a | off | off | off | off | **on (lock)** |
| `warn-integer-overflow` | n/a | off | off | off | off | **on (lock)** |
| `error-leaky-jobs` | n/a | off | off | off | off | **on (lock)** |
| `warn-leaky-jobs`/`ask-leaky-jobs` | n/a | off | off | off | off | × (lock off — MX) |
| `eval` builtin | available | available | available | available | available | **차단 (lock)** |
| Backticks 사용 가능 | ✓ | ✓ | ✓ | ✓ | ✓ | **× (lock)** |
| `(e)` re-eval 사용 가능 | × | ✓ | × | ✓ | ✓ | **× (lock)** |

## 실행 — Trap/Signal ([16](16-trap-signal.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX `trap` + EXIT | ✓ | ✓ | ✓ | ✓ | ✓ |
| DEBUG/ERR/RETURN pseudo-signal | × | ✓ | ✓ | ✓ | ✓ |
| `trap -p` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `trap -l` | × | ✓ | × | ✓ | ✓ |
| Trap stacking (`--append/--prepend`) | × | ✓ | × | ✓ | ✓ |
| `.sh.*` context vars | × | ✓ | ✓ | ✓ | ✓ |

## 실행 — Job control ([17](17-job-control.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX jobs/fg/bg/wait/kill/disown | ✓ | ✓ | ✓ | ✓ | ✓ |
| Jobspec (`%n`, `%string`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `jobs -l/-p/-r/-s/-n` | × | ✓ | ✓ | ✓ | ✓ |
| `disown -h/-a/-r` | × | ✓ | ✓ | ✓ | ✓ |
| `wait -n/-p/-f` | × | ✓ | × | ✓ | ✓ |
| `kill -n/-l NUM/-L` | × | ✓ | 일부 (-n, -l) | ✓ | ✓ |
| Leaky-jobs 3-option (warn/ask/error, MX) | × | ✓ | × | ✓ | ✓ |

## Builtins ([18](18-builtins.md))

| 카테고리 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX special + regular | ✓ | ✓ | ✓ | ✓ | ✓ |
| ksh93 (`print`/`whence`/`compound`/`nameref`/`enum`) | × | × | ✓ | ✓ | ✓ |
| `let` (ksh93/bash 공통) | × | ✓ | ✓ | ✓ | ✓ |
| bash (`mapfile`/`pushd-popd-dirs`/`caller`/`help`/`builtin`) | × | ✓ | × | ✓ | ✓ |
| kash 신규 (`die`/`assert`/`usage`) | × | ✓ | × | ✓ | ✓ |
| `read --prompt` | × | ✓ | × | ✓ | ✓ |

## 인터랙티브 ([19](19-interactive.md), [22](22-prompt.md), [23](23-history.md), [29](29-abbreviations.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| Line editor (rustyline) | — (interactive only) | — | — | — | — |
| `complete` (fish-style) | × | ✓ | × | ✓ | ✓ |
| `bind` (fish-style) | × | ✓ | × | ✓ | ✓ |
| `PS1/PS2/PS4` 변수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS3` (select) | ✓ | ✓ | ✓ | ✓ | ✓ |
| `PS0` | × | ✓ | × | ✓ | ✓ |
| `.kash.prompt`/`.right_prompt`/`.precmd`/`.preexec`/`.chpwd` | × | ✓ | × | ✓ | ✓ |
| History 자체 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `fc` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `history` subcommand | × | ✓ | × | ✓ | ✓ |
| Timestamps/cwd/exit per entry | n/a | ✓ | n/a | ✓ | ✓ |
| `!` history expansion | × | ✓ (opt-in) | × | ✓ (opt-in) | ✓ (opt-in) |
| `.kash.history.*` introspection | × | ✓ | × | ✓ | ✓ |
| `abbr` builtin | ✓ (interactive only) | ✓ | ✓ (interactive only) | ✓ | ✓ |
| Visible expansion | interactive only (모든 모드) | | | | |

## First-party utilities ([14](14-first-party-utils.md))

| Category | 모든 모드 | (any)-secure |
|---|---|---|
| Network (tcp-connect/-listen, udp-send/-recv) | ✓ | ✓ |
| try (예외 처리) | ✓ | ✓ |
| term-style | ✓ | ✓ |
| JSON 9종 (json-validate/get/set/keys/type/length/format/merge/to-compound) | ✓ | ✓ |
| Time/Date 7종 (time-now/format/parse/add/diff/duration/sleep) | ✓ | ✓ |
| Temp 4종 (mktemp + mktemp-file/dir/fifo) | ✓ | ✓ |

**중요**: 모든 first-party utility는 *mode-independent*. base mode/postfix 불문하고 가용.

## 보안 / Shellshock 정책 ([26](26-security-policy.md))

| 정책 | 모든 모드 (P1-P5는 architecture 차원) |
|---|---|
| P1: env 비-함수화 | (모든 모드) |
| P2: Rust 메모리 안전 | (모든 모드) |
| P3: 외부 데이터 묵시 eval 금지 | (모든 모드, `-secure`에서 `eval` 자체 차단) |
| P4: Function 정의 source-only | (모든 모드) |
| P5: TLS default ON | (모든 모드 — HTTP utility v2+) |

## 모드 시스템 자체 ([01](01-modes.md), [02](02-mode-syntax.md))

| 기능 | strict | aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `mode <name>` (unbounded) | × | ✓ | × | ✓ | ✓ |
| `mode -L <name>` | × | ✓ | × | ✓ | ✓ |
| `mode <name> { ... }` block | × | ✓ | × | ✓ | ✓ |
| `${.sh.mode}` introspection | × | ✓ | ✓ | ✓ | ✓ |
| Symlinks (`sh`, `ksh`) | (외부 호출) | | | | |
