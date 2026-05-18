# v1 Sweep — 누적 미결 일괄 해소

v1 설계 진행 중 각 메모리/문서에 쌓인 ~75개 미결 항목을 일괄 정리. 각 결정의 원래 출처는 해당 문서 참조.

## 카테고리 C — 사용자 결정 결과

- **C-1 (int128+uint128 promotion)**: kash가 **helper 함수 제공** — `signed-to-unsigned`, `narrow-safe`, `widen-signed`, `saturating-add`, `wrapping-add` 등으로 사용자 명시. 묵시 promotion은 동일 부호 + 작은→큰만.
- **C-2 (imaginary unit literal)**: `1+2i` **도입**. 산술 컨텍스트에서 `<numeric><i>` 패턴이 literal. 변수 `i`와 구분 (numeric prefix 필수, `1i`가 순수 imaginary unit).
- **C-3 (compound type 암묵 변환)**: **모든 모드 금지**. 변경 원하면 `unset` 후 재할당.
- **C-4 (compound vs assoc 동적 변경)**: **모든 모드 거부**.
- **C-5 (`usage` doc comment)**: `## doc` style 채택 (함수 정의 위의 `##` 시작 주석을 doc으로 인식).
- **C-6 (API stability tier)**: v1 모든 기능 동일 보장 (experimental tier 도입 안 함).

## 카테고리 B — 주요 simple commit (전부 50+ 항목 중 일부)

| 영역 | 항목 | 결정 |
|---|---|---|
| Config | 시스템 config | `/etc/kashrc` + `/etc/kashrc.d/*.kash` |
| Config | KASH_ENV | 도입 (bash 호환) |
| Config | State file | `$XDG_STATE_HOME/kash/` |
| First-party | 설치 layout | `/usr/local/bin/<utility>` symlink |
| History | nohist 이름 | `nohist` |
| History | File lock | `flock(2)` |
| History | Multi-line cmd | JSON `\n` escape |
| OOP ext | `__del` 비정상 종료 | best-effort |
| OOP ext | `__init` 실패 | 인스턴스 unset + 전파 |
| OOP ext | private nameref 우회 | nameref 생성 거부 |
| OOP ext | static multi-process | 각자 own copy |
| OOP ext | type vs 변수 충돌 | type 우선, escape `${.kash.types.X.y}` |
| Prompt | preexec scope | top-level interactive 명령만 |
| std typeclass | `yield` 인자 없음 | error |
| std typeclass | early-finalization | finally 실행 보장 |
| std typeclass | Showable fallback | JSON-like dump |
| Versioning | POSIX 트랙 selector | `--posix-track=2024` + pragma |
| Versioning | 첫 release POSIX | POSIX.1-2024 |
| Arithmetic | bfloat/f16 round | IEEE 754 round-to-nearest-even |
| Arithmetic | 함수 vs 변수 충돌 | function call 컨텍스트 우선 |
| Arrays | zsh slicing form | `${arr[@]:s..e}` (range `..`) |
| Builtins | `mapfile` 옵션 | bash 동일 |
| Builtins | `caller` 출력 | bash 호환 |
| Builtins | `select` prompt | PS3 (POSIX) |
| Expansion flags | multi-char alias | single-char juxta + multi-char separator |
| Expansion flags | `(z)/(g::)` -secure | blocked |
| Function scope | positional capture | v1 미지원 |
| Function scope | strict unfound | strict error, default unset |
| Glob | qualifier 카탈로그 | zsh 전체 |
| Glob | `^`/`~` escape | `\^`, `\~` |
| Glob | ERE backreference | 미지원 |
| Glob | `.sh.match` ↔ BASH_REMATCH | nameref |
| I/O | auto-fd 범위 | bash 호환 (10+) |
| I/O | persistent redirect | lexical scope |
| Job control | ask-leaky prompt | `[k]ill/[d]isown/[w]ait/[p]roceed?` |
| Job control | error-leaky exit | `3` (새 카테고리) |
| Mode syntax | mode 선언 위치 | 어디든 |
| Mode syntax | mode -L 안 unbounded | error |
| Mode syntax | shebang vs pragma | pragma 우선 + warning |
| Modes | 여러 modifier 조합 | 허용 (`default-secure-noglob`) |
| Namespace | use 충돌 | strict error, default shadow+warning |
| Namespace | brace wildcard | `use .ns.{*}` 명시 form |
| Namespace | re-export | `re-export` 구문 |
| Namespace | versioning | path 활용 |
| Namespace | cyclic | lazy load + cycle detect error |
| Quote | `\u{HEX}` 자릿수 | 1-6 hex (max U+10FFFF) |
| Quote | POSIX-strict 미정의 escape | error |
| Set options | warn 출력 | stderr |
| Set options | `set -o` 출력 | POSIX + bash 호환 |
| Set options | 모드 전환 옵션 | lexical 자동 복원 |
| Subshell | coproc fd naming | `${name[0]}`/`${name[1]}` |
| Trap | ERR timing | errexit 전 |
| Trap | DEBUG builtin | bash 동일 |
| Trap | `.sh.exit_status` 변경 | 호출 측 반영 |
| Trap | coproc 종료 signal | SIGTERM → 10s grace → SIGKILL |
| Trap | stacking 중복 | 허용 |
| Typeclass | `assert` 실패 | stderr + exit 1 (catchable은 `try`) |
| Typeclass | `-satisfies` transitive | yes (sub instance면 super 만족) |
| Typeclass | sourced import | caller scope (namespace 정합) |

(전체 60+ 항목은 [project_kash_sweep_v1.md](../../.claude/projects/-home-ybi-kash/memory/project_kash_sweep_v1.md) 참조)

## 카테고리 D — v2+ / impl detail defer

v2+ 검토:
- 패키지 manifest (모듈 시스템)
- `-no-network` modifier
- History fuzzy search, `history-search` utility 분리
- Mode line, abbreviation
- Multi-line prompt (rustyline 포크 시)
- `Complex` partial order
- `Cloneable`/`Reprable`/`Num`/`Default`/`Collection` 등 typeclass
- `warn-fd-leak` option
- 언어 차원 `private` modifier
- Locale-specific quoting
- API stability tier (experimental track)

Impl detail:
- Mutation 거부 정확한 error msg/exit code
- `wait -f` SIGTSTP race condition
- `-secure` lock 해제 시도 시 error msg
- Deprecation 경고 메시지 형식
- rustyline KeyCode 매핑 (`bind` 표기)
- Generator state/stack 저장 (Rust coroutine vs state machine)
- 호환 보장 위반 fix 정책 (meta-process)
