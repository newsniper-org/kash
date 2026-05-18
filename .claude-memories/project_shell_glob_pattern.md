---
name: New shell — glob and pattern matching (committed)
description: 글로빙/패턴 매칭 — extglob, recursive **, zsh 확장 syntax, glob qualifiers, brace expansion, case 종결자, regex, null glob 정책, 모드별 가용성
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 glob/pattern 시스템 확정 사항. (관련: project_shell_modes.md)

## 적용 컨텍스트

같은 패턴 syntax가 4개 컨텍스트에서 통일적으로 사용:
1. Filename expansion (`ls *.txt`)
2. `case` statement
3. `[[ x = pat ]]` test
4. Parameter expansion (`${var#pat}`, `${var/pat/repl}` 등)

## 공통 기반 (POSIX)
`*`, `?`, `[abc]`, `[!abc]` / `[^abc]`, `[a-z]`, `[[:class:]]` (POSIX character class). 셋 다 공유, 변경 없음.

## 확정 사항

### (a) Extglob — 기본 on
`@(p|q)`, `*(p)`, `+(p)`, `?(p)`, `!(p)` 모두 default-on. ksh93 정합. bash 같은 opt-in 안 함.

### (b) `**` recursive globbing — 기본 on
zsh 정합. POSIX-aware 이상에서 가용.

### (c) zsh 확장 syntax — 기본 on (default 모드)
- `^pat` 부정
- `abc~def` exclusion (mid-pattern; 시작 위치 `~`는 tilde expansion 유지)
- `<a-b>` numeric range, `<->` any number
- `#pat` 0+ 반복 (postfix), `##pat` 1+ 반복
모두 표현력 추가, 충돌 없음.

### (d) Glob qualifiers — `(#q...)` 명시 표기 강제
zsh의 `*.log(om[1])` form은 ksh93 extglob `*(pattern)`과 동일 form이라 모호. **해결**: zsh 자체의 `(#q...)` disambiguation 채택.

- `*.log(om[1])` → default 모드에서 **parse error** (모호)
- `*.log(#qom[1])` → qualifier 명시
- `*(.|backup)` → extglob

v1에 포함할 qualifier 항목:
- 파일 type: `.`(regular), `/`(dir), `@`(symlink), `=`(socket), `p`(pipe), `*`(executable)
- 권한: `r`/`w`/`x` (user), `R`/`W`/`X` (other), `s`(setuid)
- 소유: `u:user:`, `g:group:`
- 정렬: `o<key>` (asc), `O<key>` (desc); key=`n`name, `m`mtime, `a`atime, `c`ctime, `L`size
- 선택: `[N]`, `[N,M]` (sorted result에 대한 slicing)
- Modifier: `N` (null glob), `D` (include dotfiles), `Y<n>` (stop after n matches)

### (e) Brace expansion — step 포함
`{a,b,c}`, `{1..10}`, `{1..10..2}` 모두 가용. ksh93에 step 없지만 추가.

### (f) `case` 종결자 — `;;`, `;&`, `;;&` 채택
- `;;` standard (POSIX)
- `;&` fall-through (bash/zsh)
- `;;&` continue-match (bash/zsh)
POSIX는 `;;`만, 신규 종결자는 POSIX-aware 이상에서.

### (g) `=~` regex — ERE
POSIX 표준 ERE. 매치 그룹의 canonical form은 `.sh.match[]` (ksh93). bash의 `BASH_REMATCH`는 alias로 제공 (호환).

### (h) Null glob 기본 동작 — **모드별 차등**

| 모드 | Null glob 동작 |
|---|---|
| POSIX-strict | unchanged (POSIX 기본 — literal 패턴 그대로) |
| POSIX-strict-secure | **fail** |
| POSIX-aware | unchanged |
| POSIX-aware-secure | **fail** |
| ksh93u-strict | unchanged |
| ksh93u-strict-secure | **fail** |
| ksh93u-aware | unchanged (ksh93 정합) |
| ksh93u-aware-secure | **fail** |
| default | **fail** (footgun 제거 정신) |
| default-secure | **fail** |

규칙 요약: **default mode 또는 `-secure` postfix가 있으면 fail, 그 외에는 unchanged.** opt-in null glob은 qualifier `(#qN)` 으로 명시:
```sh
for f in *.txt(#qN); do ...done    # 매치 없으면 empty (반복 안 함)
```

## 모드별 가용성 요약

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 패턴 | ✓ | ✓ | ✓ | ✓ | ✓ |
| Extglob | × | ✓ | ✓ | ✓ | ✓ |
| `**` recursive | × | ✓ | ✓ | ✓ | ✓ |
| zsh 확장 syntax (`^`, `~`, `<a-b>`, `#`) | × | ✓ | × | ✓ | ✓ |
| Glob qualifiers `(#q...)` | × | ✓ | × | ✓ | ✓ |
| Brace step `{1..10..2}` | × | ✓ | × | ✓ | ✓ |
| case `;&`, `;;&` | × | ✓ | × | ✓ | ✓ |
| `=~` ERE | × | ✓ | ✓ | ✓ | ✓ |

## 미결

남은 항목:
- `<N-M>` range에서 leading zero 처리 (impl detail — 자릿수 매칭 정책)

기타 모두 project_kash_sweep_v1.md에서 해소.

**How to apply:** 향후 파일 처리/텍스트 처리 관련 설계 (예: file iteration, find-like API, 텍스트 stream 처리)는 이 패턴 시스템을 전제로. `find` 명령의 일부 기능은 이 qualifier 시스템으로 자연스럽게 흡수 가능 (`**/*.log(#q.om[1])` 등).
