# First-Party Utility CLIs

kash가 자체 제공하는 utility 명령. 첫 카테고리 — 네트워크.

⚠️ **Stability rule**: 이 문서의 interface는 lock됨. 변경은 conscious decision으로만.

## Architecture (모든 first-party utility 일관 적용)

⚠️ **모든 first-party utility는 base 모드 및 postfix를 *불문하고* 가용**.

- **kash 내부**: 모든 모드에서 in-process built-in으로 실행. fork 없음.
- **kash 외부**: kash binary의 **symlink** (busybox-multicall 패턴). argv[0]로 dispatch.
- 사용자 입장: 일반 외부 명령. 새 문법 없음.
- 모드 시스템은 *언어*만 통제, *utility 명령 vocabulary*는 통제하지 않음 (POSIX shell의 `cd`/`echo` builtin과 같은 위상).
- **이 정책은 향후 모든 first-party utility 카테고리 (JSON, HTTP, parallel, time, try-catch 등) 에 동일 적용**.

## Naming convention

**`<도메인>-<동작>`** kebab-case. 모든 first-party utility에 일관 적용.

## Common flag convention

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

## Standard streams / exit code

- **stderr**: 에러, warning, verbose 로그
- **stdout**: 결과 데이터
- **exit**: POSIX (0 성공, 1 일반 실패, 2+ 카테고리별)

## 초기 utility 4종 — 네트워크 카테고리

### `tcp-connect` — bidirectional TCP 클라이언트
```
tcp-connect [OPTIONS] HOST PORT
```
- 데이터: stdin → 서버 전송, 서버 응답 → stdout
- exit: 0 = peer graceful close, 1+ = error
- options:
  - `-4` / `-6` — IP family
  - `-t SECONDS` / `--timeout=SECONDS` — connect timeout
  - `--bind ADDR[:PORT]` — local bind

```sh
coproc conn tcp-connect example.com 80
print -p "GET / HTTP/1.0\r\n\r\n"
```

### `tcp-listen` — TCP 서버
```
tcp-listen [OPTIONS] [HOST:]PORT CMD [ARGS...]
```
- PORT listen, 연결마다 CMD 실행 (CMD의 stdin/stdout = socket)
- options:
  - `-4` / `-6`
  - `-1` / `--once` — 한 연결 수락 후 종료
  - `-N COUNT` / `--count=COUNT` — 최대 동시 연결
  - `--bind ADDR[:PORT]`

### `udp-send` — UDP 단방향
```
udp-send [OPTIONS] HOST PORT [MESSAGE]
```
- MESSAGE arg 있으면 그것을, 없으면 stdin을 하나의 datagram으로 전송
- options:
  - `-4` / `-6`
  - `--bind ADDR[:PORT]`

### `udp-recv` — UDP listener
```
udp-recv [OPTIONS] [HOST:]PORT
```
- 받은 datagram을 stdout에 출력
- options:
  - `-4` / `-6`
  - `-1` / `--once` 또는 `-N COUNT` — 받을 개수
  - `-d DELIM` — datagram 사이 구분자 (기본 `\n`)
  - `--bind ADDR[:PORT]`

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| 4개 네트워크 utility | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

모든 모드에서 가용. 향후 `-no-network` 같은 modifier가 추가되면 그때 차단 검토.

## Transpiler 매핑

bash의 `/dev/tcp/...`, `/dev/udp/...` → utility 호출:

| bash | kash transpile |
|---|---|
| `exec 3<>/dev/tcp/H/P` | `coproc _c tcp-connect H P; exec 3<>${_c[0]}` (or 적절한 wrapping) |
| `cat </dev/tcp/H/P` | `tcp-connect H P < /dev/null` |
| `cmd >/dev/udp/H/P` | `cmd \| udp-send H P` |

## 터미널 스타일 — `term-style` (locked)

ANSI/SGR escape sequence 출력 utility (fish `set_color` 패턴).

```
term-style [OPTIONS] [STYLE...]
```

### STYLE (positional)
- 8 basic colors: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`
- 8 bright: `bright-black`, ..., `bright-white`
- Attributes: `bold`, `dim`, `italic`, `underline`, `blink`, `reverse`, `strikethrough`
- Special: `reset`

### OPTIONS
- `--fg COLOR`, `--bg COLOR`
- `--hex '#RRGGBB'`, `--bg-hex '#RRGGBB'`
- `--rgb R G B`, `--bg-rgb R G B`
- `--256 N`, `--bg-256 N`
- `--no-bold`, ..., `--no-strikethrough`
- `--reset`
- `--check` (지원 여부 → exit code)
- `--force` (NO_COLOR/non-TTY 무시)
- `--auto` (default in 인터랙티브)

### Detection
1. `NO_COLOR` env → no color
2. `TERM=dumb`/미설정 → no color
3. 비-TTY → no color
4. 그 외 → color OK

[no-color.org](https://no-color.org/) 준수.

### Exit code
- 0 정상 / `--check` color 지원
- 1 `--check` color 미지원
- 2 잘못된 인자

### 사용
```sh
print "$(term-style red bold)error$(term-style reset)"
print "$(term-style --hex '#ff00aa' bold)hot pink$(term-style reset)"
```

## JSON 카테고리 — 9개 utility (locked)

Path syntax: **JSON Pointer (RFC 6901)**. JSONC 입력 지원 (`//`, `/* */`, trailing commas — VS Code 컨벤션). 출력은 strict JSON. `--strict-json`로 JSONC 거부.

### 9개 utility

| Utility | 역할 | 주요 옵션 |
|---|---|---|
| `json-validate [FILE]` | 검증 (exit code) | `--strict-json` |
| `json-get PATH [FILE]` | 값 추출 | `-r/--raw`, `-p/--pretty`, `-c/--compact`, `--default VAL` |
| `json-set PATH VALUE [FILE]` | 수정 | `-s/--string`, `-i/--in-place`, `--no-create` |
| `json-keys PATH [FILE]` | 키/인덱스 enumerate | `--sort`, `--null` |
| `json-type PATH [FILE]` | type 이름 출력 | |
| `json-length PATH [FILE]` | object/array/string 길이 | |
| `json-format [FILE]` | pretty/compact | `--compact`, `--indent N`, `--sort-keys` |
| `json-merge FILE...` | 깊은 병합 | `--array-mode=replace\|concat\|merge-by-index`, `--type-mismatch=...`, `-i` |
| `json-to-compound [FILE]` | kash compound literal | `--into-var NAME` (in-process only), `--strict-identifier` |

### 모든 JSON utility 공통
- `--strict-json` — JSONC features 거부
- 표준 공용 flag (`-h/-V/-v/-q`)

### Exit code (모든 JSON utility)
- 0: 성공
- 1: path 없음 / 검증 실패 / type mismatch 등
- 2: 잘못된 인자

### 사용 예
```sh
# JSONC OK
json-get '/config/host' settings.jsonc

# 수정 후 stdout
json-set '/version' '"1.2.3"' package.json > updated.json

# in-place atomic
json-set -i '/version' '"1.2.3"' package.json

# compound 변환
config=$(json-to-compound config.json)
# 또는 in-process
json-to-compound --into-var config config.json

# 검증
json-validate --strict-json release.json && deploy
```

## Time/Date 카테고리 (locked) — 7개 utility

POSIX `date` 보완. 내부 repr = Unix epoch float (ns precision).

### Format presets (공통)
`iso8601` (default), `iso8601-utc`, `rfc3339`, `rfc2822`, `unix` (s), `unix-ms`, `unix-us`, `unix-ns`, `date`, `time`, `datetime`, 또는 custom strftime (`%` 포함 시 인식).

### Duration presets (공통)
`human` (default — "2h 30m"), `iso8601` ("P1DT2H30M"), `seconds`, `milliseconds`. Parse 시 자동 감지.

### 7개 utility

| Utility | 역할 |
|---|---|
| `time-now [--format/--utc/--zone/--precision]` | 현재 시간 |
| `time-format EPOCH [...]` | epoch → 포맷된 문자열 |
| `time-parse STRING [--format/--zone/--utc]` | 문자열 → epoch |
| `time-add EPOCH DURATION` | epoch + duration |
| `time-diff EPOCH1 EPOCH2 [--format ...]` | 두 epoch 차이 (default human) |
| `time-duration ARG [--to ...]` | duration 변환 (parse/format 자동) |
| `time-sleep DURATION` | POSIX sleep보다 ergonomic (1h30m 등) |

### TZ
IANA name (`Asia/Seoul`, `America/New_York`). `TZ` env 자동 인식.

### Exit code
- 0 정상 / 1 parse 실패 / 2 잘못된 인자

### 사용 예
```sh
time-now                                    # ISO 8601 local
time-now --format unix-us                   # 마이크로초 epoch
time-add $(time-now --format unix) 1h       # 1시간 후
time-diff $start $end --format human        # "2h 30m"
time-sleep 1m30s                            # 90초
```

## Temp resource 카테고리 (locked) — 4개 utility

### `mktemp` — POSIX + bash superset CLI

POSIX `mktemp` ∪ bash `mktemp`의 common strict superset. 기존 스크립트 무수정 호환.

지원: `-d`/`-q`/`-u`/`-p DIR`/`--tmpdir`/`-t TEMPLATE`/`--suffix`/`--dry-run` + positional template.

kash 신규 flag (`--auto-cleanup` 등)는 `mktemp`가 아닌 `mktemp-*`에서.

### `mktemp-file [OPTIONS]` / `mktemp-dir [OPTIONS]` / `mktemp-fifo [OPTIONS]`

- Atomic 생성 (file/fifo `O_EXCL`, dir `mkdir(2)`)
- 기본 모드: file/fifo 0600, dir 0700

### `mktemp-*` 공통 OPTIONS

| Flag | 의미 |
|---|---|
| `-p DIR` / `--dir DIR` | 부모 (default `$TMPDIR`→`/tmp`) |
| `-t TEMPLATE` / `--template TPL` | template (XXXXXX → random) |
| `--prefix PREFIX` | random 앞 |
| `--suffix SUFFIX` | random 뒤 |
| `-m MODE` / `--mode MODE` | octal mode |
| `--auto-cleanup` | 셸 종료 시 자동 정리 (in-process only) |
| `--auto-cleanup-var VAR` | path를 VAR에 + auto-cleanup (in-process only) |
| `-q` / `--quiet` | 에러 억제 |

### Auto-cleanup

- **In-process**: cleanup handler에 등록, 셸/함수/블록 종료 시 자동 삭제
- **외부 symlink + `--auto-cleanup*` 지정**: **실행 거부** (silent footgun 방지)

### Exit code
- 0 성공 / 1 생성 실패 / 2 잘못된 인자 (auto-cleanup 외부 사용 포함)

### 사용 예
```sh
# POSIX 호환
f=$(mktemp -d --suffix=.work)

# kash canonical
mktemp-file --auto-cleanup-var f --suffix .log
# $f 사용, 자동 cleanup
```

## 향후 카테고리 (commit 전, 별도 결정)

- HTTP: `http-get`, `http-post`
- 병렬 실행: `parallel-run`
- YAML / TOML

각 utility 도입 시 *사용자 승인 후* commit. naming/flag/exit policy는 일관 유지.

## 미결

- 설치 layout 정확한 위치
- 외부 호출 시 PATH 자동 등록 메커니즘
- in-process vs external symlink 호출의 환경 동일성 보장 정책
- `tcp-listen` connection limit 초과 시 동작
- `udp-recv -d`에서 datagram 자체에 delimiter 포함 시 처리
- `-no-network` modifier 도입 여부
