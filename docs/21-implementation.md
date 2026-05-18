# Implementation Language and Key Dependencies

## 구현 언어 — Rust

- rustyline (Q1에서 line editor로 결정) 이 Rust 라이브러리
- Rust 채택의 사실상 함의: 메모리 안전성, 단일 정적 바이너리 (busybox-multicall에 적합), 모던 ecosystem

## Line editor — rustyline 18.0.0 fork

- Upstream: rustyline 최신 안정버전 **18.0.0** 에서 fork
- Fork 목적: **`no_std` 대응** (embedded, 정적 링킹, WASM, bootstrap 시나리오)
- `alloc` crate은 사용 (heap-allocated 자료구조 필요)
- std-only 기능 (file system, threads, env)은 wrapper trait로 추상화
- 세부는 [19-interactive.md](19-interactive.md)

## Rust 툴체인 — Linux는 musl ABI 필수

모든 Linux target은 **musl libc ABI** 로 빌드:
- 단일 정적 binary 배포 (busybox-multicall 패턴 정합)
- glibc 버전 의존성 회피 → 어느 배포판에서도 동일 binary
- alpine/distroless 같은 minimal 환경 native

기본 Linux targets:
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `armv7-unknown-linux-musleabihf`
- `riscv64gc-unknown-linux-musl`

비-Linux는 native libc 사용 (macOS Darwin, Windows MSVC, BSD).

## Implementation 결정 (committed)

| 영역 | 결정 |
|---|---|
| Parser/AST | Hand-written recursive descent (no external dep) |
| Async runtime | v1 미도입 (sync 기반) |
| Serialization | `serde` + `serde_json` (`serde_json_core` fallback for no_std) |
| Crate 구조 | Cargo workspace (아래 목록) |
| Multicall dispatch | argv[0] → `phf` compile-time HashMap |
| Tests | unit + integration + insta snapshot + cargo-fuzz + POSIX/bash corpus |
| Error reporting | source location + color caret (term-style 활용) |
| Logging | `tracing` crate, `KASH_LOG=debug` |
| Crash | friendly panic + bug report, `KASH_BACKTRACE=1` for backtrace |

### Workspace crates

```
kash-workspace/
├── kash-core/                       # 셸 엔진
├── kash-line-editor/                # rustyline 18.0.0 fork (upstream contribute-back)
├── kash-utils-net/                  # tcp/udp utility
├── kash-utils-json/                 # json-*
├── kash-utils-time/                 # time-*
├── kash-utils-temp/                 # mktemp-*
├── kash-utils-misc/                 # term-style, try
├── kash-transpile-bash/
├── kash-transpile-inputrc/
├── kash-transpile-bash-completion/
└── kash-bin/                        # multicall dispatch
```

### Line editor — upstream contribute-back

`kash-line-editor`는 **rustyline upstream에 PR할 것을 염두에 둔 design**:
- 변경 minimal/upstream-ready
- kash-specific hack은 fork가 아니라 kash-core wrapper에
- `no_std` 대응, multi-line prompt 같은 일반 개선은 upstream에 PR
- 장기적으로 fork = thin diff or zero

Fork-and-forget 회피 → 유지보수 부담 감소 + Rust 생태계 기여.

## 빌드/배포

- Release: `cargo build --release --target=<TARGET>`
- Linux musl ABI 필수
- Single static binary (multicall)
- CI: Linux musl 다수 arch, macOS, Windows
- 패키지: tarball 1차, distro 차후
