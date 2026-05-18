---
name: kash — implementation language and key dependencies (committed)
description: 구현 언어는 Rust, rustyline을 line editor 기반으로. 향후 추가 dependencies는 별도 결정.
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash *구현* (implementation) 차원의 큰 결정들. 지금까지는 설계/스펙 위주였으나 line editor 결정에서 시작해 구현 측면 commitment가 시작됨.

## 구현 언어 — Rust

- **이유**: rustyline (Q1에서 line editor로 결정) 이 Rust 라이브러리. rustyline 활용으로 자연스럽게 Rust 생태계 채택.
- Rust 선택 자체가 사용자에 의해 직접 명시된 건 아니지만 rustyline 채택의 사실상 함의.
- 메모리 안전성, 단일 정적 바이너리 (busybox-style multicall에 적합), 모던 ecosystem.

## Line editor — rustyline fork (committed)

- **Upstream**: rustyline 최신 안정버전 **18.0.0** 에서 fork.
- **Fork 목적**: **`no_std` 대응** — Rust 표준 라이브러리 없이 빌드 가능하도록.
- readline-like API + Rust 생태계.
- Highlighter / hinter / completer trait API → fish 스타일 autosuggestion/highlight 매핑.

### Upstream contribute-back 정책 (committed)

`kash-line-editor` fork는 **upstream rustyline에 contribute-back할 것을 염두에 둔 design**:
- 변경 minimal하고 upstream-ready하게
- kash-specific hack은 fork에 직접 넣지 말 것 — kash-core 쪽에서 wrapper로 처리
- `no_std` 대응, multi-line prompt 등 *일반적으로 유용한* 개선은 upstream에 PR
- Upstream maintainer (kkawakam) 와 협력 가능한 형태 유지
- 장기적으로는 fork 자체가 thin diff 또는 0이 되도록

Fork-and-forget 회피 — 유지보수 부담 감소 + Rust 생태계에 기여.

### `no_std` 함의

`no_std` 채택은 의미 있는 architectural 결정:
- kash core 자체도 가능한 `no_std`/`alloc`-only 노선 검토 (embedded target, 정적 링킹 최소화, WASM, bootstrap shell 시나리오 등)
- std에 의존하는 기능 (file system, threads, env)은 별도 layer로 격리
- `alloc` crate은 사용 가능 (Vec/String/Box 등 heap-allocated 자료구조 필요)
- 표준 라이브러리의 std::process, std::fs 등은 wrapper trait로 추상화

### 부족한 부분 보완

- zsh-style multi-line prompt
- kash widget API
- 기타 인터랙티브 기능 → fork에서 직접 patch

## Rust 툴체인 설정 (committed)

### Linux target — musl ABI 필수

모든 Linux target은 **반드시 musl libc ABI 기준으로 빌드**:
- 정적 링킹 → 단일 binary 배포 가능 (busybox-multicall 패턴과 정합)
- glibc 버전 의존성 회피 → 어느 Linux 배포판에서도 동일 binary 동작
- alpine/distroless 같은 minimal 환경에서도 native
- 기본 Linux target들:
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `armv7-unknown-linux-musleabihf`
  - `riscv64gc-unknown-linux-musl`
  - 기타 — 필요에 따라 추가

### 비-Linux target

- macOS: `aarch64-apple-darwin`, `x86_64-apple-darwin` (system libc)
- Windows: `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` (MSVC ABI 권장)
- BSD: `x86_64-unknown-freebsd` 등 (system libc)
- WASM: `wasm32-unknown-unknown` (no_std 채택과 정합 — 향후 검토)

musl 강제는 *Linux에 한정*. 다른 OS는 native libc.

## Implementation 결정 (committed)

### Parser/AST — hand-written recursive descent
- 최대 유연성, REPL error message 우수, `no_std` 친화
- chumsky/nom/lalrpop 외부 의존 회피

### Async runtime — v1 미도입
- 대부분 sync syscall로 충분
- 필요 시 thread / `select`/`poll`
- tokio는 무거움 + `no_std` 불가
- v2+에서 `smol` 등 가벼운 옵션 검토

### Serialization — `serde` + `serde_json`
- `no_std` 빌드는 `serde_json_core` fallback

### Crate 구조 — Cargo workspace

```
kash-workspace/
├── kash-core/                       # 셸 엔진 (parser, eval, builtin)
├── kash-line-editor/                # rustyline 18.0.0 fork (upstream contribute-back 친화)
├── kash-utils-net/                  # tcp-connect, udp-*
├── kash-utils-json/                 # json-*
├── kash-utils-time/                 # time-*
├── kash-utils-temp/                 # mktemp-*
├── kash-utils-misc/                 # term-style, try
├── kash-transpile-bash/             # bash → kash
├── kash-transpile-inputrc/          # .inputrc → kash bind
├── kash-transpile-bash-completion/  # bash complete → kash complete
└── kash-bin/                        # main binary (multicall dispatch)
```

### Multicall dispatch — argv[0] hashmap lookup
- `phf` crate으로 compile-time HashMap
- argv[0] basename → utility entry function
- 매치 없으면 kash-core shell 모드

### Test 전략
- Unit tests (cargo test)
- Integration tests (subprocess 실행 검증)
- Snapshot tests (`insta` crate)
- Fuzz tests (`cargo-fuzz`) — parser/JSON/glob (보안 정책 P2 검증)
- POSIX conformance suite + bash compat corpus

### Error reporting
- `miette` 같은 diagnostic 라이브러리 또는 자체 — source location + color caret
- `term-style` utility 활용 (색상)

### Logging / Debug
- `tracing` crate (구조화 로그, span)
- Release는 disabled, debug 빌드만 verbose
- `KASH_LOG=debug` env var control
- Crash: friendly panic + bug report 안내, `KASH_BACKTRACE=1` 시 backtrace

## 빌드/배포

- Release: `cargo build --release --target=<TARGET>`
- Linux: musl ABI 필수 (committed)
- Single static binary (multicall)
- CI: Linux musl 다수 arch, macOS, Windows
- 패키지: tarball 1차, distro 차후

**How to apply:** kash 구현 시 위 정책 기본. 추가 implementation decision (새 dependency, CI tool, build system 변경 등) 시 사용자 승인 후 진행. fork (rustyline 등)는 upstream contribute-back 친화 design 유지.
