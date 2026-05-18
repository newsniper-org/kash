---
name: kash — v1 sweep of accumulated 미결 items (committed)
description: 각 메모리 파일에 누적된 미결 항목들의 일괄 해소. v1 spec ship-ready 만드는 청산.
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
v1 설계 진행 중 각 메모리에 쌓인 미결 항목들을 일괄 정리한 sweep. 모든 결정은 본 문서에 기록 + 원본 메모리 파일의 미결 section은 *해당 항목 제외*.

## 카테고리 B — Simple commit (clear answer)

### Config (project_kash_config.md)
- **시스템 config 경로**: `/etc/kashrc` + `/etc/kashrc.d/*.kash` (user layout 거울)
- **KASH_ENV 도입**: bash `BASH_ENV` 호환 — non-interactive script에서 env 통해 init script 지정
- **State file 위치**: XDG Base Dir 준수 — `$XDG_STATE_HOME/kash/` (history 등), `$XDG_CACHE_HOME/kash/`

### First-party utilities (project_kash_first_party_utils.md)
- **설치 layout**: `/usr/local/bin/<utility>` symlink (FHS 표준 위치)
- **In-process vs external 환경 동일성**: best-effort 동일 (env vars, working dir, exit code 동일 보장). 차이점은 매뉴얼 문서화.
- **tcp-listen connection limit 초과**: OS 기본 accept queue (Linux/Mac POSIX) — 추가 큐잉 없음.
- **udp-recv -d delimiter conflict**: datagram 내 delimiter 발생 시 그대로 출력 (사용자 책임 — datagram 경계는 protocol level에서 보장).

### History (project_kash_history.md)
- **nohist 옵션 이름**: `nohist` (현재 prompt부터 history 저장 안 함)
- **File lock**: `flock(2)` POSIX (cross-platform — Windows는 OS-specific)
- **Multi-line command JSON**: JSON string의 `\n` escape (single JSON line 유지)

### Interactive (project_kash_interactive.md)
- **Completion 경로 우선순위**: `$XDG_CONFIG_HOME/kash/completions/` > `/usr/share/kash/completions/`

### OOP extensions (project_kash_oop_extensions.md)
- **`__del` 비정상 종료**: best-effort — `kill -9`/segfault 같은 즉시 종료에서는 호출 보장 없음. 정상/SIGTERM 등 catchable 종료는 호출.
- **`__init` 실패 (non-zero exit) 정책**: 인스턴스 unset, error 호출 측으로 전파.
- **`private` nameref 우회**: `typeset -n r=x._private` *생성 자체를 거부* (parse/runtime error).
- **`static` 멤버 multi-process**: 각자 own copy (POSIX subshell 의미와 일관).
- **Type vs 변수 이름 충돌**: compound access 컨텍스트에서 type 우선. 명시적 escape: `${.kash.types.Foo.bar}`.

### Prompt (project_kash_prompt.md)
- **`.kash.prompt` 출력**: stdout (확정).
- **`.kash.preexec` scope**: top-level interactive 명령에만. 함수 안 명령은 호출 안 됨.
- **VI mode indicator**: rustyline 포크 시 추가 (v2+ 시점에 구체화).

### Standard typeclasses (project_kash_std_typeclasses.md)
- **`yield` 인자 없음**: error (간단/안전).
- **Generator early-finalization**: caller가 iter 도중 break 시 generator의 finally 블록 실행 보장.
- **Generator state 구현**: Rust 차원 결정 — impl 단계 (project_kash_implementation.md).
- **`for x in iterable_var`**: parse-time에 변수 type instance lookup, 못 찾으면 POSIX word-split fallback.
- **Compound Showable fallback**: JSON-like dump (`{key: value, ...}` 형태) — Showable instance 없을 시.

### Versioning (project_kash_versioning.md)
- **POSIX 트랙 selector**: `--posix-track=2024` CLI flag + pragma `posix-track 2024`.
- **첫 release POSIX**: **POSIX.1-2024** (최신).

### Arithmetic (project_shell_arithmetic.md)
- **bfloat/f16 round**: IEEE 754 round-to-nearest-even (standard).
- **Math 함수 vs 변수 충돌**: 함수 호출 컨텍스트 (`func(args)`)에서 함수 우선, 그 외 변수.
- **int128 + uint128 promotion → C-1 (사용자 결정)**: implicit promotion *안 함*. 대신 **kash가 helper 함수 제공** — `signed-to-unsigned`, `narrow-safe`, `widen-signed`, `widen-unsigned`, `saturating-add`, `wrapping-add` 등 (정확한 이름/set는 first-party utility로 별도 commit). 사용자가 *명시적으로* 타입 강제. 묵시적 promotion은 같은 부호 + 작은→큰 만.

### Arrays (project_shell_arrays.md)
- **zsh slicing 새 form**: `${arr[@]:s..e}` (range 표기 `..` — Rust 영향).
- **`typeset -i arr` attribute 전파**: 원소 전체에 일관 적용. 명시.
- **Empty array check canonical**: `${#arr[@]} == 0`.

### Builtins (project_shell_builtins.md)
- **warn-echo-portability 추가**: warn-* 패밀리 확장, `-secure` lock 추가.
- **`usage` doc comment → C-5**: **`## doc` style 채택** — 함수 정의 바로 위의 `##` 시작 주석을 doc으로 인식.
  ```kash
  ## brief description
  ## detailed help
  ## usage: myfunc ARG1 ARG2
  function myfunc { ... }
  
  usage myfunc      # 위 `##` 주석을 출력
  ```
- **`mapfile` 옵션**: bash 동일 (`-t/-n/-O/-s/-c/-C/-u`).
- **`caller` 출력**: bash 호환 (`LINE FILE FUNC`).
- **`select` prompt**: POSIX PS3 그대로.

### Compound vars (project_shell_compound_vars.md)
- **멤버 type 변환 → C-3 (사용자 결정)**: **compound type 암묵 변환 일절 금지 (모든 모드)**. `person.x=5` (scalar) 후 `person.x=(...)` 시도 → 모든 모드에서 **error**. 변경 원하면 `unset person.x` 후 재할당.
- **`typeset -n ref=person` 후 `ref.x` type 호환성**: nameref target의 type 따름.
- **Compound vs assoc 동적 변경 → C-4 (사용자 결정)**: **모든 모드에서 거부** (b 선택). `typeset -A var` 후 `typeset -C var` 시도 → error.

### Expansion flags (project_shell_expansion_flags.md)
- **multi-char alias parsing**: single-char juxtaposition은 individual flag (zsh 호환), multi-char 이름은 공백/콤마 separator 필수.
- **`(z)`/`(g::)` -secure 처리**: blocked in `-secure` (eval-like 위험 — security policy P3 일관).

### Function scope (project_shell_function_scope.md)
- **nameref transitive read-only**: underlying readonly 전염.
- **Positional param capture**: v1 미지원 (named var만).
- **Capture list trailing comma**: 허용 (style 자유).
- **Strict 모드 unfound capture**: strict modes error, default modes unset (POSIX `$undeclared` 의미).

### Glob/pattern (project_shell_glob_pattern.md)
- **Qualifier 카탈로그**: zsh 전체 (file type/perm/owner/sort/select/modifier 카테고리 — 기존 명시된 것 + zsh의 나머지 전부).
- **`^`/`~` escape**: backslash로 literal (`\^`, `\~`).
- **`<N-M>` 음수**: 미지원 (양수 정수 range만).
- **ERE backreference**: 미지원 (POSIX ERE 정합).
- **`.sh.match` ↔ `BASH_REMATCH`**: nameref alias.

### I/O redirection (project_shell_io_redirection.md)
- **Auto-fd 범위**: bash 호환 (10 이상).
- **Persistent redirect scope**: lexical 따름. `exec 3>file`이 함수/블록 안에서 set되면 그 scope 내에서만 유효, exit 시 자동 close.

### Job control (project_shell_job_control.md)
- **`ask-leaky-jobs` prompt UI**: `[k]ill / [d]isown / [w]ait / [p]roceed?` 한 줄 prompt.
- **`error-leaky-jobs` exit code**: **`3`** (새 카테고리 — POSIX 미정의 range).
- **non-interactive ask → error 격상 메시지**: `kash: error-leaky-jobs: <N> background jobs not waited or disowned at exit` (stderr).
- **Coprocess가 leaky-job 카테고리 포함**: 포함 (확인).

### Mode syntax (project_shell_mode_syntax.md)
- **`mode` 선언 위치**: 어디든 가능 (zsh-like 유연).
- **`mode -L` 안 `mode <name>` (unbounded)**: **error** (monotonicity 일반화 — lexical 보호 깰 수 없음).
- **Shebang `--mode=` vs file pragma `mode` 충돌**: pragma 우선 (이미 명시) + stderr warning 제공.

### Modes (project_shell_modes.md)
- **여러 modifier 조합**: 허용 (`default-secure-noglob` 같은).

### Namespace (project_shell_namespace.md)
- **`use namespace` 충돌**: strict mode error, default mode shadow + stderr warning.
- **Brace wildcard**: `use .ns.{*}` 명시 형식만 (` use .ns.*` 같은 ambiguous form은 거부).
- **Re-export**: 별도 `re-export namespace bar` 또는 `re-export .ns.symbol` 구문 도입.
- **Namespace versioning**: namespace path로 처리 (`use .org.example.v2.utils`). 별도 versioning 시스템 없음.
- **Cyclic namespace**: lazy load + cycle detection error.

### Quote handling (project_shell_quote_handling.md)
- **`$'...'` `\u{HEX}` 자릿수**: 1-6 hex digits, max `U+10FFFF`.
- **POSIX-strict 미정의 escape**: error (예: `\?` 가 POSIX-2024에 없으면 strict에서 거부).

### Set options (project_shell_set_options.md)
- **`warn-*` 출력 위치**: stderr.
- **`set -o` 단독 호출 출력**: POSIX + bash 호환 형식 (옵션 이름 + on/off).
- **모드 전환 시 옵션 상태**: lexical scope 따라 자동 복원 (mode -L exit 시).

### Subshell/pipeline (project_shell_subshell_pipeline.md)
- **`coproc` fd naming**: `${name[0]}`/`${name[1]}` (bash 호환).

### Trap/signal (project_shell_trap_signal.md)
- **ERR trap timing**: errexit 전 (POSIX 권장).
- **DEBUG trap builtin 발동**: bash 동일 — 모든 simple command (builtin/external 무관).
- **`.sh.exit_status` 변경**: trap 안에서 변경 가능, 호출 측에 반영.
- **Coproc 종료 signal sequence**: SIGTERM → 10초 grace → SIGKILL.
- **Trap stacking 중복 추가**: 허용 (idempotent 옵션 없음 v1).

### Typeclass (project_shell_typeclass.md)
- **`assert` 실패 동작**: stderr error 메시지 + `exit 1`. Catchable은 `try` utility로.
- **`-satisfies` transitive**: yes — Ord requires Eq면 Ord instance 보유 type은 Eq도 만족.
- **Sourced file typeclass import**: caller scope에 들어옴 (namespace 결정과 일관).

### Arithmetic (계속)
- **bcomplex32 vs complex32 doc 강조**: doc 명시 (bfloat16 mantissa/exponent 배분 차이 강조 — ML 용도).

## 카테고리 C — 사용자 결정 결과 (위 본문에 통합)

- **C-1**: helper 함수 제공으로 사용자 자체 처리. 묵시 promotion은 동일 부호 + 작은→큰 만.
- **C-2**: imaginary unit 리터럴 `1+2i` **도입**. 규칙:
  - 산술 컨텍스트 (`$((...))`, `((...))`, `let`)에서 `<numeric><i>` 패턴이 imaginary literal
  - 예: `2i`, `3.14i`, `0.5i`, `1+2i`
  - 변수 `i`와 구분: numeric prefix 있으면 literal, 없으면 변수
  - `1i` = 순수 imaginary unit (변수 `i`와 구분 위해 `1` prefix 필요)
- **C-3**: compound type 암묵 변환 **모든 모드 금지**.
- **C-4**: compound vs assoc 동적 변경 **모든 모드 거부**.
- **C-5**: `## doc` comment 컨벤션 채택.
- **C-6**: v1 모든 기능 동일 호환 보장 (experimental tier 도입 안 함).

## 카테고리 D — v2+로 이동 / impl detail defer

### v2+ 후속 검토
- config: 패키지 manifest / locking 메커니즘
- first-party: PATH 자동 등록, `-no-network` modifier
- history: fuzzy search, `history-search` utility 분리
- interactive: mode line, abbreviation (별도 결정 round)
- prompt: multi-line prompt (rustyline 포크 후)
- std_typeclasses: `complex` partial order, `Cloneable`/`Reprable`/`Num`/`From`/`Default`/`Collection` 등
- io_redirection: `warn-fd-leak` option, process subst fd 누수 진단
- namespace: 언어 차원 `private`
- quote: locale-specific quoting
- versioning: API stability tier

### Impl detail defer (구현 단계 결정)
- mutation 거부 정확한 error 메시지/exit code
- `wait -f` SIGTSTP race condition
- `-secure` lock 옵션 해제 시도 시 정확한 error 메시지
- Deprecation 경고 메시지 형식
- `interactive: complete -n COND` 매칭 의미 (rustyline 구현)
- `bind` key sequence 표기 (rustyline KeyCode 매핑)
- 호환 보장 위반 fix 정책 (project meta-process)
- ksh93u-strict negative index 정확한 error 메시지
- `tcp-listen` connection limit 초과 정확한 동작 (현재: OS queue)
- Generator state/stack 저장 구현 (Rust coroutine/state machine/OS thread)

**How to apply:** 각 원본 메모리 파일의 미결 section은 본 sweep에서 해소된 항목을 제외하고, 남은 v2+/impl detail만 유지. 본 sweep 메모리는 *원본 메모리 결정의 자세한 출처*로 항상 reference.
