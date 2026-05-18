---
name: kash — first-party utility CLIs (committed, locked interfaces)
description: kash가 자체 제공하는 utility 명령 — 초기 네트워크 4종(tcp-connect/tcp-listen/udp-send/udp-recv)과 naming/interface/common convention. Stability contract.
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash가 자체 제공하는 *first-party utility CLI*들의 design contract.

⚠️ **Stability rule** (feedback_first_party_util_stability.md): 이 메모리에 lock된 interface는 임의로 변경 금지. 변경 필요 시 사용자 명시 승인 후에만.

## Architecture (전역 정책) — 모든 first-party utility에 일관 적용

⚠️ **모든 first-party utility는 base 모드와 postfix를 *불문하고* 가용**. 모드 시스템은 *언어*만 통제, *utility 명령 vocabulary*는 통제하지 않음. POSIX shell이 `cd`/`echo`를 builtin으로 가지는 것과 같은 위상.

- **kash 내부 호출**: 모든 모드에서 in-process built-in으로 실행. fork 없음.
- **kash 외부 호출**: kash binary의 **symlink** (busybox/coreutils-multicall 패턴). argv[0]로 invocation 분기.
  - 예: `/usr/local/bin/tcp-connect` → `/usr/local/bin/kash`
  - kash가 자기 argv[0]을 보고 utility 모드로 진입
- 사용자 입장에서는 외부 명령처럼 invocation. 새 문법 없음.
- 이 정책은 **현재 4개 네트워크 utility** 뿐 아니라 **향후 추가될 모든 first-party utility 카테고리 (JSON, HTTP, parallel, time, try-catch 등) 에 동일하게 적용**.

## Naming convention (lock됨)

**`<도메인>-<동작>`** kebab-case. 모든 first-party utility가 따라야 함.

예: `tcp-connect`, `tcp-listen`, `udp-send`, `udp-recv`. 향후 추가될 utility들(JSON, HTTP, parallel 등)도 동일 컨벤션.

## Common flag convention (lock됨)

모든 first-party utility에 일관 적용:

| 단축 | 장형 | 의미 |
|---|---|---|
| `-h` | `--help` | 도움말 |
| `-V` | `--version` | 버전 |
| `-v` | `--verbose` | 상세 로그 |
| `-q` | `--quiet` | 최소 출력 |
| `-4` / `-6` | `--ipv4` / `--ipv6` | IP family |
| `-t N` | `--timeout=N` | 타임아웃 (초) |
| `-N N` | `--count=N` | 반복 횟수 |
| `-1` | `--once` | one-shot 모드 |

## Standard streams / exit code 정책 (lock됨)

- **stderr**: 모든 에러 메시지, warning, verbose 로그
- **stdout**: utility의 *결과 데이터* 만
- **exit code**: POSIX 컨벤션
  - `0` = 성공
  - `1` = 일반 실패
  - `2+` = 카테고리별 (utility별 매뉴얼에 명세)

## 초기 utility 4종 (네트워크 카테고리) — interface lock됨

### `tcp-connect` — bidirectional TCP 클라이언트
```
tcp-connect [OPTIONS] HOST PORT
```
- **데이터 흐름**: stdin → 서버로 전송, 서버 응답 → stdout
- **exit**: 0 = peer graceful close, 1+ = error
- **options**:
  - `-4` / `-6` — IP family
  - `-t SECONDS` / `--timeout=SECONDS` — connect timeout
  - `--bind ADDR[:PORT]` — local bind 주소
- **typical pattern**:
  ```sh
  coproc conn tcp-connect example.com 80
  print -p "GET / HTTP/1.0\r\n\r\n"
  ```

### `tcp-listen` — TCP 서버
```
tcp-listen [OPTIONS] [HOST:]PORT CMD [ARGS...]
```
- PORT에서 listen, 연결마다 CMD 실행 (CMD의 stdin/stdout = socket)
- **options**:
  - `-4` / `-6`
  - `-1` / `--once` — 한 연결 수락 후 종료
  - `-N COUNT` / `--count=COUNT` — 최대 동시 연결 수
  - `--bind ADDR[:PORT]`

### `udp-send` — UDP 단방향 송신
```
udp-send [OPTIONS] HOST PORT [MESSAGE]
```
- MESSAGE arg 있으면 그것을, 없으면 stdin을 **하나의 datagram**으로 전송
- **options**:
  - `-4` / `-6`
  - `--bind ADDR[:PORT]`

### `udp-recv` — UDP listener
```
udp-recv [OPTIONS] [HOST:]PORT
```
- 받은 datagram을 stdout에 출력
- **options**:
  - `-4` / `-6`
  - `-1` / `--once` 또는 `-N COUNT` — 받을 개수
  - `-d DELIM` — datagram 사이 구분자 (기본 `\n`)
  - `--bind ADDR[:PORT]`

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| 4개 네트워크 utility | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

모든 모드에서 가용 — utility는 *언어 기능*이 아니라 *셸 환경*의 일부. 향후 `-no-network` 같은 modifier가 추가되면 그때 차단 검토 (현재 `-secure`는 네트워크 차단 안 함).

## Transpiler 매핑 (참고용)

bash의 `/dev/tcp/...`, `/dev/udp/...` 패턴 → utility 호출로 변환:

| bash | kash transpile |
|---|---|
| `exec 3<>/dev/tcp/H/P` | `coproc _c tcp-connect H P; exec 3<>${_c[0]}` (or 적절한 wrapping) |
| `cat </dev/tcp/H/P` | `tcp-connect H P < /dev/null` |
| `cmd >/dev/udp/H/P` | `cmd \| udp-send H P` |

세부 fd 추적 패턴은 구현. transpiler 메모리(`project_shell_transpiler.md`)와 일관.

## 터미널 스타일 utility — `term-style` (locked)

ANSI/SGR escape sequence 출력 utility. fish `set_color` 패턴.

```
term-style [OPTIONS] [STYLE...]

STYLE (positional, 여러 개 가능):
  # Foreground 8 basic + 8 bright
  black, red, green, yellow, blue, magenta, cyan, white
  bright-black, bright-red, ..., bright-white
  
  # Attributes
  bold, dim, italic, underline, blink, reverse, strikethrough
  
  reset                  # 특수 — 전부 reset

OPTIONS:
  --fg COLOR             # 명시 foreground (positional override)
  --bg COLOR             # background named color
  --hex '#RRGGBB'        # truecolor fg (24-bit)
  --bg-hex '#RRGGBB'     # truecolor bg
  --rgb R G B            # truecolor fg (R/G/B 0-255)
  --bg-rgb R G B
  --256 N                # 256-color palette fg (0-255)
  --bg-256 N             # 256-color palette bg
  --no-bold, --no-dim, --no-italic, --no-underline,
  --no-blink, --no-reverse, --no-strikethrough
  --reset                # 전부 reset
  --check                # color 지원 여부 → exit code (출력 없음)
  --force                # NO_COLOR/non-TTY 무시
  --auto                 # NO_COLOR/non-TTY면 빈 출력 (default in 인터랙티브)
```

### Detection 규칙 (`--check`/`--auto`)
1. `NO_COLOR` env set → no color
2. `TERM=dumb` 또는 미설정 → no color
3. stdout 비-TTY → no color
4. 그 외 → color OK

`--force`로 1-3 무시. https://no-color.org/ 준수.

### Exit code
- `0`: 정상 / `--check` 시 color 지원
- `1`: `--check` 시 color 미지원
- `2`: 잘못된 인자 (unknown color/attr, hex 형식 오류, RGB 범위 초과 등)

### 사용
```sh
print "$(term-style red bold)error$(term-style reset)"
print "$(term-style --hex '#ff00aa' bold)hot pink$(term-style reset)"
print "$(term-style red --bg blue bold underline)test$(term-style reset)"

if term-style --check; then
    red=$(term-style red); nc=$(term-style reset)
fi
```

## 예외 처리 utility — `try` (locked)

```
try [OPTIONS]

OPTIONS:
  --do BLOCK              (필수) 시도할 명령들 — string으로 묶음
  --catch [VAR] BLOCK     실패 시 실행. VAR 지정 시 error msg/exit code 캡쳐
  --okay BLOCK            성공 시 실행 (--do가 0으로 끝났을 때)
  --finally BLOCK         항상 실행 (성공/실패 무관, --do 후)
  -e, --exit-var VAR      try 전체 최종 exit code를 VAR에 저장
```

### 의미론
- 4 phase: **do** → (성공 시 **okay**, 실패 시 **catch**) → **finally**
- Exit code: catch가 처리하면 0, catch 없거나 catch도 실패하면 그 exit code, finally 실패 시 finally 우선
- Block은 kash 코드 string
  - 내부 호출 시 caller의 scope에서 evaluation (변수 공유)
  - 외부 호출 시 별도 kash subprocess (scope 격리 — 외부 호출의 본질적 한계)
- 단일 단어 이름 (`<도메인>-<동작>` 컨벤션 예외) — concept name이 단어 하나로 충분히 단일.

## JSON 카테고리 (locked) — 9개 utility

Path syntax: **JSON Pointer (RFC 6901)** — `/foo/bar/0`, `~1` = `/`, `~0` = `~`. jq DSL 대체 목적 아님 — 기본 navigation/manipulation 위주.

**JSONC 입력 지원 (모든 utility)** — Microsoft VS Code 컨벤션 (line `//` + block `/* */` comments + trailing commas). 출력은 항상 strict JSON. `--strict-json` flag로 JSONC features 거부.

### 9개 utility

#### `json-validate [OPTIONS] [FILE]`
- 검증, exit code만 (출력 없음)
- `0` valid (JSONC default), `1` invalid (stderr line/col)
- `--strict-json` — 순수 RFC 8259 JSON 검증

#### `json-get [OPTIONS] PATH [FILE]`
- 값 추출
- `-r`/`--raw` — string 값 outer quote 제거
- `-p`/`--pretty` (default) / `-c`/`--compact`
- `--default VALUE` — path 없으면 VALUE (exit 0)
- exit: `0` 성공, `1` path 없음, `2` 잘못된 인자

#### `json-set [OPTIONS] PATH VALUE [FILE]`
- 수정, stdout default
- VALUE: JSON parse (bare string은 `'"hello"'`)
- `-s`/`--string` — VALUE를 JSON parse 안 함, string 강제
- `-i`/`--in-place` — file 직접 수정 (atomic via temp+rename)
- path 선조 객체 자동 생성. `--no-create`로 비활성.

#### `json-keys [OPTIONS] PATH [FILE]`
- object 키 / array 인덱스 enumerate (한 줄당 하나)
- `--sort` — 정렬
- `--null` — null separator

#### `json-type [OPTIONS] PATH [FILE]`
- "object", "array", "string", "number", "boolean", "null"
- path 없으면 exit 1

#### `json-length [OPTIONS] PATH [FILE]`
- object 키 수 / array 원소 수 / string UTF-8 codepoint 수
- 다른 type → exit 1

#### `json-format [OPTIONS] [FILE]`
- pretty default (indent 2)
- `--compact` / `-c`
- `--indent N`
- `--sort-keys`

#### `json-merge [OPTIONS] FILE1 FILE2 [FILE3...]`
- 깊은 병합 (객체 키 재귀, 충돌 시 뒤 우선)
- `--array-mode=replace|concat|merge-by-index` (default replace)
- `--type-mismatch=error|right-wins|left-wins` (default right-wins)
- `-i`/`--in-place FILE` (default FILE1)

#### `json-to-compound [OPTIONS] [FILE]`
- kash compound literal로 변환
- 예: `(name="Alice" age=30 tags=("admin" "user"))`
- `--into-var NAME` — in-process 호출 시 caller 변수 직접 설정 (외부 호출 시 error)
- key가 식별자 규칙 위반 시 assoc array fallback. `--strict-identifier`로 error.

### 모든 utility 공통 flag (위 standard 외)
- `--strict-json` — JSONC 거부

## Time/Date 카테고리 (locked) — 7개 utility

POSIX `date`의 modern 보완. 내부 repr = Unix epoch float (ns precision).

### Format presets (모든 utility 공통)
| 이름 | 예시 |
|---|---|
| `iso8601` (default) | `2026-05-17T13:30:00+09:00` |
| `iso8601-utc` | `2026-05-17T04:30:00Z` |
| `rfc3339` | `2026-05-17T13:30:00.123456+09:00` |
| `rfc2822` | `Sun, 17 May 2026 13:30:00 +0900` |
| `unix` | `1747448400` (seconds) |
| `unix-ms` | `1747448400123` (milliseconds) |
| `unix-us` | `1747448400123456` (microseconds) |
| `unix-ns` | `1747448400123456789` (nanoseconds) |
| `date` | `2026-05-17` |
| `time` | `13:30:00` |
| `datetime` | `2026-05-17 13:30:00` |
| (커스텀 strftime) | `%Y...` 패턴 — `%` 포함 시 strftime |

### Duration presets
| 이름 | 예시 |
|---|---|
| `human` (default) | `2h 30m 5s`, `1d 12h`, `1w 2d` |
| `iso8601` | `P1DT2H30M5S` |
| `seconds` | `9005` |
| `milliseconds` | `9005000` |

Parsing 시 자동 감지 (numeric → seconds, `P...` → ISO 8601, 그 외 → human).

### 7개 utility

#### `time-now [OPTIONS]`
- `--format`, `--utc`, `--zone TZ`, `--precision N`
- Default: ISO 8601 local TZ

#### `time-format EPOCH [OPTIONS]`
- 옵션 `time-now`과 동일

#### `time-parse STRING [OPTIONS]`
- `--format` (strptime/preset, default auto-detect), `--zone`, `--utc`
- 출력: epoch

#### `time-add EPOCH DURATION [OPTIONS]`
- DURATION 자동 감지
- 음수 가능 (`-1h`)

#### `time-diff EPOCH1 EPOCH2 [OPTIONS]`
- Default 출력: human duration
- `--format human|iso8601|seconds|milliseconds`

#### `time-duration ARG [OPTIONS]`
- 자동: 숫자→format, 문자열→parse
- `--to seconds|milliseconds|human|iso8601`

#### `time-sleep DURATION [OPTIONS]`
- POSIX `sleep`보다 ergonomic (`1h30m`, `500ms`, `P30S` 등)
- POSIX `sleep` 그대로 호환 유지 (외부)

### TZ 처리
- IANA TZ database
- `--zone Asia/Seoul` 등
- `TZ` 환경변수 자동 인식

### Exit code (모든 time utility)
- 0: 정상
- 1: parse 실패 / 잘못된 TZ
- 2: 잘못된 인자

## Temp resource 카테고리 (locked) — 4개 utility

### `mktemp` — POSIX + bash superset CLI

POSIX `mktemp` ∪ bash `mktemp`의 *common strict superset*. 기존 bash/POSIX 스크립트 무수정 호환 (sh 심볼릭링크와 같은 superset 정신).

지원 flag (POSIX + bash union):
- `-d` — directory 모드 (POSIX)
- `-q` — quiet on error (POSIX)
- `-u` — unsafe (이름만 출력, 생성 안 함, bash; **race condition 위험** — 사용 비권장)
- `-p DIR`, `--tmpdir[=DIR]` — 부모 디렉토리 (bash)
- `-t TEMPLATE` — TMPDIR 안에서 template 사용 (bash; POSIX와 의미 약간 다름 — bash 의미 우선)
- `--suffix=SUFFIX` (bash)
- `--dry-run` (alias for `-u`)
- (POSITIONAL TEMPLATE)

**kash-specific 신규 flag는 `mktemp`에 추가 안 함** — `mktemp-*` 신규 utility 사용 권장.

### `mktemp-file [OPTIONS]`
임시 파일 (atomic, `O_EXCL`). 기본 모드 `0600`.

### `mktemp-dir [OPTIONS]`
임시 디렉토리 (`mkdir(2)` atomic). 기본 모드 `0700`.

### `mktemp-fifo [OPTIONS]`
임시 named pipe. 기본 모드 `0600`.

### `mktemp-*` 공통 OPTIONS (kash-canonical)

```
-p DIR | --dir DIR             # 부모 (default: $TMPDIR → /tmp)
-t TEMPLATE | --template TPL   # template (XXXXXX → random; default: kash.XXXXXX)
--prefix PREFIX                # random part 앞 prefix
--suffix SUFFIX                # random part 뒤 suffix (e.g., '.json')
-m MODE | --mode MODE          # octal mode
--auto-cleanup                 # 셸 종료 시 자동 정리 (in-process only)
--auto-cleanup-var VAR         # path를 VAR에 저장 + auto-cleanup (in-process only)
-q | --quiet                   # 에러 메시지 억제
```

표준 공용 flag: `-h/--help`, `-V/--version`, `-v/--verbose`

### Auto-cleanup 동작

- **In-process** (kash 내부 호출): cleanup handler에 등록, 셸/함수/블록 종료 시 자동 삭제. trap EXIT보다 fine-grained scope.
- **외부 symlink 호출 시 `--auto-cleanup` 또는 `--auto-cleanup-var` 지정**: **실행 거부** (parse/init error). silent footgun 방지 — 사용자가 auto-cleanup 작동한다고 오해할 수 있음.

### Template

- `XXXXXX` (6+ X) → random 치환
- `--prefix PRE --suffix SUF` 조합 가능
- Default: `kash.XXXXXX`

```sh
mktemp-file                                # /tmp/kash.aB3xY7
mktemp-file --suffix .log                  # /tmp/kash.aB3xY7.log
mktemp-file --prefix mylog- --suffix .txt   # /tmp/mylog-aB3xY7.txt
mktemp-dir --auto-cleanup-var d             # in-process, $d에 path, 자동 cleanup
```

### POSIX `mktemp` 호환 패스

- POSIX-strict/POSIX-aware: `mktemp` superset 인터페이스 (POSIX features만 사용 시 무차이)
- `mktemp-*` 신규 utility는 모든 모드 가용 (first-party utility mode-independent 정책)
- Transpiler: bash `mktemp -d` → 그대로 `mktemp -d` 가능 (superset), 또는 `--idiomatic` 시 `mktemp-dir`

### Exit code

- 0: 성공
- 1: 생성 실패
- 2: 잘못된 인자 (auto-cleanup 외부 사용 포함 — 새 exit category로 고려)

### 향후 추가 검토 utility 카테고리 (별도 commit 필요)

- **HTTP (v2+ 연기)**: `http-get`, `http-post` 등 — Shellshock 정책 (project_kash_security_policy.md) 적용된 baseline 위에서 v2+에 진행
- YAML / TOML

### 안 제공으로 결정된 카테고리

- **병렬 실행 (`parallel-run`)** — `just`, `make`, GNU `parallel` 등 외부 도구가 이미 잘 만들어져 있음. 셸 내장은 NIH. (async/await 영구 보류 — feedback_no_async_await_until_posix.md — 과 일관: POSIX `&`/`wait` + 외부 도구로 충분.)

각 utility 도입 시 *반드시* 사용자에게 interface 확정 받은 뒤 commit. naming convention과 common flag는 일관 유지.

## 미결

남은 v2+ 항목:
- 외부 호출 시 PATH 자동 등록 메커니즘 (distro packaging 결정)
- `-no-network` modifier 도입 — network access 차단용 별도 modifier (v2+)

기타 모두 project_kash_sweep_v1.md에서 해소 — 설치는 `/usr/local/bin/<utility>` symlink, 환경 동일성 best-effort, tcp-listen은 OS queue, udp-recv는 사용자 책임.

**How to apply:** 신규 first-party utility 도입 시 (1) 위 naming convention 따름, (2) common flag convention 일관 적용, (3) stdout/stderr/exit code 정책 준수, (4) interface는 lock 전에 반드시 사용자 승인. 기존 utility interface 변경 제안 시 stability rule 따라 push back.
