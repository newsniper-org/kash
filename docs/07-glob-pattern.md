# Glob and Pattern Matching

## 적용 컨텍스트

같은 패턴 syntax가 4개 컨텍스트에서 통일적으로 사용:
1. Filename expansion (`ls *.txt`)
2. `case` statement
3. `[[ x = pat ]]` test
4. Parameter expansion (`${var#pat}`, `${var/pat/repl}` 등)

## 공통 기반 (POSIX)
`*`, `?`, `[abc]`, `[!abc]` / `[^abc]`, `[a-z]`, `[[:class:]]` (POSIX character class). 변경 없음.

## 확정 사항

### Extglob — 기본 on
`@(p|q)`, `*(p)`, `+(p)`, `?(p)`, `!(p)` 모두 default-on. ksh93 정합.

### `**` recursive globbing — 기본 on
zsh 정합.

### zsh 확장 syntax — 기본 on (default 모드)
- `^pat` 부정
- `abc~def` exclusion (mid-pattern; 시작 위치 `~`는 tilde expansion 유지)
- `<a-b>` numeric range, `<->` any number
- `#pat` 0+ 반복 (postfix), `##pat` 1+ 반복

### Glob qualifiers — `(#q...)` 명시 표기 강제

zsh의 `*.log(om[1])` form은 ksh93 extglob `*(pattern)`과 동일 form이라 모호. zsh 자체의 `(#q...)` disambiguation 채택:

- `*.log(om[1])` → default 모드에서 **parse error** (모호)
- `*.log(#qom[1])` → qualifier 명시
- `*(.|backup)` → extglob

#### v1 qualifier 항목

- 파일 type: `.`(regular), `/`(dir), `@`(symlink), `=`(socket), `p`(pipe), `*`(executable)
- 권한: `r`/`w`/`x` (user), `R`/`W`/`X` (other), `s`(setuid)
- 소유: `u:user:`, `g:group:`
- 정렬: `o<key>` (asc), `O<key>` (desc); key=`n`name, `m`mtime, `a`atime, `c`ctime, `L`size
- 선택: `[N]`, `[N,M]` (sorted result에 대한 slicing)
- Modifier: `N` (null glob), `D` (include dotfiles), `Y<n>` (stop after n matches)

### Brace expansion — step 포함
`{a,b,c}`, `{1..10}`, `{1..10..2}` 모두 가용.

### `case` 종결자
- `;;` standard (POSIX)
- `;&` fall-through (bash/zsh)
- `;;&` continue-match (bash/zsh)

### `=~` regex — ERE
POSIX 표준 ERE. 매치 그룹의 canonical form은 `.sh.match[]` (ksh93). bash의 `BASH_REMATCH`는 alias로 제공.

### Null glob 기본 동작 — 모드별 차등

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

규칙: **default mode 또는 `-secure` postfix가 있으면 fail, 그 외에는 unchanged.**

opt-in null glob은 qualifier `(#qN)`으로 명시:
```sh
for f in *.txt(#qN); do ...done    # 매치 없으면 empty (반복 안 함)
```

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| POSIX 기본 패턴 | ✓ | ✓ | ✓ | ✓ | ✓ |
| Extglob | × | ✓ | ✓ | ✓ | ✓ |
| `**` recursive | × | ✓ | ✓ | ✓ | ✓ |
| zsh 확장 syntax | × | ✓ | × | ✓ | ✓ |
| Glob qualifiers `(#q...)` | × | ✓ | × | ✓ | ✓ |
| Brace step `{1..10..2}` | × | ✓ | × | ✓ | ✓ |
| case `;&`, `;;&` | × | ✓ | × | ✓ | ✓ |
| `=~` ERE | × | ✓ | ✓ | ✓ | ✓ |

## 미결

- Qualifier 항목 완전 카탈로그
- `^`, `~`의 escape 규칙
- Multi-pattern union/intersect 결합 규칙
- `<N-M>` range에서 음수, leading zero 처리
- ERE의 backreference 지원 여부
- `.sh.match` 와 `BASH_REMATCH`의 alias 관계가 nameref인지 별도 변수 동기화인지
