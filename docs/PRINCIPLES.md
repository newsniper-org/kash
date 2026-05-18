# kash 설계 원칙

kash v1 spec 전체에 걸친 핵심 원칙. 각 결정의 근거는 본 문서를 참조.

## 1. 우선순위 — POSIX > ksh93u+m > zsh

세 셸 의미론이 충돌할 때 적용되는 tiebreaker:
1. **POSIX-compliance** (최강 요구사항)
2. **ksh93u+m 문법 baseline** (grammar만, 소스코드와 무관)
3. **zsh 기능** (위 두 가지 baseline 위에 layered)

세부: [00-overview.md](00-overview.md), [01-modes.md](01-modes.md)

## 2. 새 설계지 복각이 아니다

ksh93의 quirks / bugs / bad design을 보존할 필요 없음 — 더 나은 쪽으로 결정. 호환성 깨지지 않는 한 ksh93/bash의 알려진 함정은 *fix*.

예: ksh93의 subshell 최적화로 인한 관찰 가능한 부수효과는 *POSIX 의미론을 따름* — 모든 모드에서.

## 3. Footgun 제거 정신

모호한 default보다 *명확한 error* 또는 *명시적 opt-in*.

대표 사례:
- `default` 모드: fail-glob (POSIX literal pattern 대신)
- `default` 모드: pipeline 마지막 cmd는 현재 shell (silent variable loss 회피)
- Cross-type access (assoc with `.`, compound with arbitrary key) → strict 모드 error
- `[[ -is ]]`/`[[ -satisfies ]]`/`assert` — 명시적 type 약속

## 4. Mode 시스템으로 의미론 충돌 격리

5개 base mode (POSIX-strict/aware, ksh93u-strict/aware, default) + modifier (`-secure` 등) — 같은 syntax가 다른 모드에서 *다른 의미*를 가지지 않음. 모드 간 차이는 *기능 가용성* (strict) 또는 *런타임 dial preset* (aware) 차원.

세부: [01-modes.md](01-modes.md), [02-mode-syntax.md](02-mode-syntax.md)

## 5. First-party utility — mode-independent

`tcp-connect`, `json-*`, `time-*`, `try`, `term-style` 등 모든 first-party utility는 *base mode와 postfix를 불문하고 가용*. 모드는 *언어*만 통제, *utility vocabulary*는 통제하지 않음 (POSIX의 `cd`/`echo` builtin과 같은 위상).

세부: [14-first-party-utils.md](14-first-party-utils.md)

## 6. bash/zsh 호환은 transpiler — runtime shim 없음

bash-specific (`complete -F`, `.inputrc`, `PS1='\u@\h'`, `$"..."` 등)은 *전적으로 transpiler가 담당*. kash core는 canonical (fish-style 등) form만 가짐. 입력 형식별 별개 transpiler:
- `bash2kash` (script)
- `inputrc2kash` (`.inputrc` → fish-style `bind`)
- `bash-completion2kash` (`complete -F` → fish-style `complete`)
- Transpiler plugin 시스템 — plugin은 kash script 파일 (`.kash`)
- 첫 plugin: gettext (`$"..."` → `$(gettext ...)`)

세부: [12-transpiler.md](12-transpiler.md), [19-interactive.md](19-interactive.md)

## 7. Shellshock 5원칙 (security baseline)

CVE-2014-6271 등 환경변수→코드 실행 부류 취약점 *원천 봉쇄*:

- **P1**: 환경변수는 절대 코드로 해석되지 않음 (bash `export -f` 비호환)
- **P2**: Parser 메모리 안전 (Rust)
- **P3**: 외부 데이터 묵시 eval 금지 (`-secure`에서 `eval` 자체 차단)
- **P4**: Function 정의는 source code에서만
- **P5**: TLS default ON, 우회는 명시적 + `-secure`에서 거부

세부: [26-security-policy.md](26-security-policy.md)

## 8. First-party utility stability rule

kash 자체 제공 utility (`tcp-connect`, `json-get` 등)의 *naming/interface는 stable contract*. 한번 lock되면 임의 변경 금지. 변경 필요 시 사용자 명시 승인 후.

세부: 메모리 `feedback_first_party_util_stability.md`, [14-first-party-utils.md](14-first-party-utils.md)

## 9. Versioning — `v<semver>-posix<연도>`

`vN-posixS` 사용자 코드는 `v(N+2)-posixS` 까지 작동 보장. POSIX 표준 개정은 별도 트랙. 첫 release는 `v1.0.0-posix2024`.

세부: (versioning 메모리), [28-sweep-v1.md](28-sweep-v1.md)

## 10. Capability는 typeclass, lifecycle은 dunder

OOP 확장에서:
- **Dunder** (`__init`, `__del`): type 정의 자체에 속하는 lifecycle hook만
- **Typeclass** (`Showable`, `Eq`, `Ord`, `Hashable`, `Iterable`, `Callable`): 외부에서 retroactively 추가 가능한 capability

이 분리로 *외부에서 행위 추가 가능* (Rust/Haskell 노선).

세부: [09-typeclass.md](09-typeclass.md), [24-oop-extensions.md](24-oop-extensions.md), [27-std-typeclasses.md](27-std-typeclasses.md)

## 11. async/await 영구 보류 (POSIX 채택까지)

`async`/`await`/`go`/`spawn` 류 키워드는 *POSIX 최신 개정판에 정식 포함되기 전까지 구현 검토조차 안 함*. POSIX `&` + `wait` + `wait -p` 조합으로 충분 — invention 비용 회피.

`yield` 키워드는 별개 (iteration 전용, 동시성 아님).

세부: 메모리 `feedback_no_async_await_until_posix.md`, [17-job-control.md](17-job-control.md)

## 12. Implementation 원칙 (Rust + musl + multicall)

- **언어**: Rust
- **Line editor**: rustyline 18.0.0 fork (no_std 지원, upstream contribute-back 친화)
- **Linux ABI**: musl 필수 (정적 binary, distro-agnostic)
- **배포**: single multicall binary (`kash` + utility symlinks)
- **Async runtime**: v1 미도입 (sync 베이스)
- **Parser**: hand-written recursive descent

세부: [21-implementation.md](21-implementation.md)

## 13. Modifier monotonicity

`-secure` 등 modifier는 *안쪽으로 갈수록 monotonically non-decreasing*. Inner scope에서 modifier 제거 불가 — 안전성 silent 풀림 방지.

세부: [02-mode-syntax.md](02-mode-syntax.md)

## 14. Bash 호환 `sh`/`ksh` symlinks

symlink는 단순 mode 초기화 아니라 *drop-in CLI 호환*:
- `sh` → posix-strict + POSIX sh CLI 정확 매칭
- `ksh` → ksh93u-strict + ksh93u+m CLI 정확 매칭

기존 bash/POSIX/ksh 스크립트 무수정 호환.

세부: [02-mode-syntax.md](02-mode-syntax.md)

## 15. Module resolution은 kash core, management는 frontend

kash core는 *resolution + loading*만 책임:
- File path → namespace 자동 매핑
- Search path 검색
- Module load + registry

Management (install, version, dependency)는 *kash 외부 frontend* 책임. Slackware-style 완전 수동 관리 기반.

세부: [30-module-resolution.md](30-module-resolution.md)
