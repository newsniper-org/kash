# v2+ Roadmap

v1에서 명시적으로 *defer*된 모든 항목 통합. v2+ release 또는 별도 결정 round에서 검토.

각 항목의 deferral 사유 + 어디에서 결정되었는지 출처 표시.

## v2 후속 검토 (정책상 v2에서 진행)

### HTTP utility 카테고리
- `http-get`, `http-post`, `http-put`, `http-patch`, `http-delete`, `http-head`, `http-options` (7종)
- 출처: [14-first-party-utils.md](14-first-party-utils.md), Shellshock 정책 결정 (사용자 선택 — v2 연기)
- 사유: Shellshock 정책 baseline 위에서 신중 design 필요. v1 lock 안 함.

### YAML / TOML utility 카테고리
- `yaml-get/set`, `toml-get/set` 등
- 출처: [14-first-party-utils.md](14-first-party-utils.md)
- 사유: JSON 우선 lock, 다른 format은 demand 보고

### Generator state 구현 옵션 (`yield` 키워드 backend)
- Rust coroutine (nightly) vs state machine vs OS thread
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md), [28-sweep-v1.md](28-sweep-v1.md)
- 사유: impl detail — Rust 안정성 향상 후 결정

### 표준 typeclass 추가
- `Cloneable`, `Reprable` (별도 — Showable과 분리), `Num`/`Add`/`Mul`/`Div` 등 산술 operator overload, `From`/`Into`, `Default`, `Collection`, `Container`, `PartialOrd` (complex 등)
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md)
- 사유: 정확한 의미론 강한 opinion 필요. v1은 6 prelude로 좁게 시작.

### Iterable / for-loop 고급 기능
- 무한 generator의 take/drop 패턴 — kash 차원 idiomatic syntax
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md)
- 사유: v1은 기본 generator + for-loop, 패턴은 utility로 검토

### Callable parser sugar
- 현재 `c.call args` 명시. `c args` 묵시는 v2+.
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md)
- 사유: parser 모호성 처리 design 필요

### Imaginary unit 추가 표기
- `1+2j` (engineering 표기)?
- 출처: [25-quote-arithmetic.md](25-quote-arithmetic.md) (v1: `i` only)
- 사유: 추가 conflict 가능성 분석 필요

### `complex` partial order typeclass (`PartialOrd`)
- IEEE 754 complex는 total order 없음 — 별도 typeclass 검토
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md)

### Generic types (typeset -T)
- `typeset -T List<T> = ...` 같은 generic
- 출처: [08-typeset.md](08-typeset.md), [09-typeclass.md](09-typeclass.md)
- 사유: 추가 표현력 — v1 보류, demand 보고 결정

### Type composition (`typeset -T C=A+B`)
- mixin 스타일 type 합성
- 출처: [08-typeset.md](08-typeset.md)
- 사유: typeclass와의 직교성 design 필요

### 언어 차원 `private` access modifier
- 현재는 `_prefix` 컨벤션
- 출처: [10-namespace.md](10-namespace.md), [30-module-resolution.md](30-module-resolution.md)
- 사유: typeset OOP `private` 확장과 통합 후 결정

### Multi-line prompt
- rustyline 18.0.0 fork에서 patch
- 출처: [22-prompt.md](22-prompt.md), [19-interactive.md](19-interactive.md)
- 사유: rustyline upstream contribute-back 우선 시도, 안 되면 fork patch

### VI mode indicator
- fish `fish_mode_prompt` 대응
- 출처: [22-prompt.md](22-prompt.md)
- 사유: rustyline 통합 시점에 검토

### Abbreviation 확장
- General position abbr (첫 단어 외)
- Trigger customize
- Universal vs session scope
- 출처: [29-abbreviations.md](29-abbreviations.md)
- 사유: v1은 fish-compat 핵심만, 확장은 demand 보고

### Mode line (terminal status line)
- 출처: [19-interactive.md](19-interactive.md)
- 사유: v1 인터랙티브 범위 외

### `history-search` first-party utility
- Fuzzy search 알고리즘 도입
- 출처: [23-history.md](23-history.md)
- 사유: fzf-like dependency 검토 필요

### History fuzzy search 자체
- 출처: [23-history.md](23-history.md)
- 사유: 외부 도구 (`fzf`)와의 관계 design

### `warn-fd-leak` option
- Process subst, exec 후 close 안 한 fd 진단
- 출처: [13-io-redirection.md](13-io-redirection.md), [11-subshell-pipeline.md](11-subshell-pipeline.md)
- 사유: 정확한 진단 알고리즘 필요

### `warn-module-conflict` option
- Search path 중 같은 module이 여러 경로에 있을 때 warning
- 출처: [30-module-resolution.md](30-module-resolution.md)
- 사유: 운영 패턴 보고 v1/v2 결정

### `-no-network` modifier
- network 사용 자체 차단
- 출처: [14-first-party-utils.md](14-first-party-utils.md)
- 사유: `-secure` 진화 검토 시 함께

### Locale-specific quoting (RTL 등)
- 출처: [25-quote-arithmetic.md](25-quote-arithmetic.md)
- 사유: 국제화 영역, demand 보고

### "Untrusted scope" 개념 (mode modifier)
- CGI 등 untrusted env 명시적 marking
- 출처: [26-security-policy.md](26-security-policy.md)
- 사유: 보안 정책 진화 시 검토

### Sandbox 모드 (`-sandboxed`?)
- fork/exec/FS access 제한
- 출처: [26-security-policy.md](26-security-policy.md)
- 사유: 별도 capability system design 필요

### API stability tier (experimental track)
- v1은 모든 기능 동일 보장
- 출처: (versioning 메모리)
- 사유: 운영 후 stability tier 도입 검토

### 자동 distribution / dependency 해소
- 출처: [30-module-resolution.md](30-module-resolution.md)
- 사유: *의도된* 외부 frontend 영역 — kash core는 핵심 안 함

### Manifest 표준 형식
- 출처: [30-module-resolution.md](30-module-resolution.md)
- 사유: frontend별 자체 결정

### Package manifest / locking 메커니즘
- 출처: [20-config.md](20-config.md), [30-module-resolution.md](30-module-resolution.md)
- 사유: 모듈 시스템 위에 frontend

### bash `mktemp -u` (unsafe)
- 출처: [14-first-party-utils.md](14-first-party-utils.md) (POSIX+bash superset에 포함은 됨)
- 사유: race condition 위험 — 가능하면 deprecation warning 검토

### Resource limit 기본값
- max recursion depth, max heredoc nesting, max process count
- 출처: [26-security-policy.md](26-security-policy.md)
- 사유: impl + 운영 결정

## 영구 보류 (POSIX 채택 시까지)

### async/await 키워드 (`async`, `await`, `go`, `spawn` 등)
- 출처: 메모리 `feedback_no_async_await_until_posix.md`
- 사유: POSIX 표준에 정식 채택되기 전까지 구현 검토 자체 하지 않음. POSIX `&` + `wait` + `wait -p` 조합으로 충분.

## Implementation detail defer (구현 시 결정)

### Parser
- 정확한 error recovery 전략
- AST → bytecode 중간 표현 도입 여부

### Mutation 거부 정확한 error msg/exit code
- 출처: [03-function-scope.md](03-function-scope.md)

### `wait -f` SIGTSTP race condition
- 출처: [17-job-control.md](17-job-control.md)

### `-secure` lock 해제 시도 정확한 error msg
- 출처: [15-set-options.md](15-set-options.md)

### Deprecation 경고 메시지 형식
- 출처: (versioning 메모리)

### rustyline KeyCode 매핑 (`bind` 표기)
- 출처: [19-interactive.md](19-interactive.md)

### `complete -n COND` 매칭 정확한 의미
- 출처: [19-interactive.md](19-interactive.md)

### ksh93u-strict negative index 정확한 error 메시지
- 출처: [04-arrays.md](04-arrays.md)

### `tcp-listen` connection limit 초과 동작
- 현재: OS queue
- 출처: [14-first-party-utils.md](14-first-party-utils.md)

### Plugin interface API (transpiler)
- 잠정 `.transpiler.plugin.register-pattern` 등
- 출처: [12-transpiler.md](12-transpiler.md)

### bash-completion 변환 완성도 기준
- 출처: [12-transpiler.md](12-transpiler.md)

### Generator state/stack 저장 (Rust coroutine vs state machine vs OS thread)
- 출처: [27-std-typeclasses.md](27-std-typeclasses.md)

### 호환 보장 위반 fix 정책 (meta-process)
- 출처: (versioning 메모리)

### File lock 메커니즘 cross-platform
- 출처: [23-history.md](23-history.md) (flock(2) baseline POSIX)

### `<N-M>` glob range leading zero 처리
- 출처: [07-glob-pattern.md](07-glob-pattern.md)

### Auto-fd 정확한 fd 할당 범위 (bash는 10+)
- 출처: [13-io-redirection.md](13-io-redirection.md)

### Multi-line command JSON 표현 세부
- 현재: escape newline
- 출처: [23-history.md](23-history.md)

## v2 release 시 trigger

다음 조건 중 하나가 가시화되면 v2 작업 시작 검토:
- 실 사용에서 v1 한계 명확화
- POSIX 표준 개정 (POSIX.1-2024 후속)
- Demand가 충분한 v2+ 항목 누적
- Implementation 경험으로 design 개선 필요

각 항목 도입 시 별도 lock round 진행 (stability rule 적용).
