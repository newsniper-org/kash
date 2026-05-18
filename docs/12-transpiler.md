# Bash → kash Transpiler and Transpiling REPL

## 쉘 이름 — **kash**

지금까지 "신규 쉘"로 부르던 것의 정식 이름.

- 정규 호출명: `kash`
- 심볼릭 링크: `sh` (posix-strict + POSIX sh CLI), `ksh` (ksh93u-strict + ksh93u+m CLI)
- bash 호환은 **symlink가 아니라 transpiler로 제공**

## Transpiler

### 입력 / 출력
- 입력: bash 4.3 이상 스크립트
- 출력: kash 스크립트 (default 모드 실행 가능)

### 호출 (잠정)
```sh
kash --transpile script.bash > script.kash
kash --transpile --idiomatic script.bash > script.kash
```

### Mapping 전략 — 두 모드

1. **Minimal rewrite (기본)**: 의미가 다른 구문만 최소 변경. 원본 그대로 유지.
2. **Idiomatic (--idiomatic)**: kash 관용구로 적극 변환.

### 변환 필수 항목 (의미 충돌)

| bash 구문 | kash 변환 |
|---|---|
| `cmd1 \|& cmd2` (stderr+stdout pipe) | `cmd1 2>&1 \| cmd2` |
| `BASH_REMATCH[i]` | `.sh.match[i]` |
| `BASHPID` | kash equivalent (잠정 `${.sh.pid}`) |
| `BASH_VERSION` | `${.sh.version}` |
| `shopt -s xxx` | `set -o xxx` 또는 `mode` 선언 |

### 변환 권장 항목 (--idiomatic flag)

| bash | kash idiomatic |
|---|---|
| `${var,,}` lowercase | `${(L)var}` |
| `${var^^}` uppercase | `${(U)var}` |
| `mapfile -t arr < file` | `arr=( ${(f)"$(<file)"} )` 또는 kash native |
| `printf -v var fmt args` | `var=$(printf fmt args)` 또는 kash equivalent |
| `[[ -v var ]]` | `[[ -n ${var+x} ]]` 또는 kash equivalent |
| `declare -A` | `typeset -A` |
| `declare -n` | `typeset -n` |

### Unmappable 항목 처리

- **warn-and-best-effort** (기본): 변환 불가 구문 만나면 warning + 유사하게 변환
- `--strict` 플래그로 error 격상
- 불확실한 변환에 `# TRANSPILER: <설명>` 주석 자동 삽입

### 호환성 보장 범위

- bash 4.3+ baseline (associative arrays, `[[ -v ]]`, `mapfile`, named `coproc` 모두 도입됨)
- bash 5.x 추가 기능도 가능한 한 매핑
- bash 3.x 이하 미지원

## 입력 형식별 별개 transpiler

bash 호환 대상은 입력 문법 자체가 다르므로 **별개 transpiler 필요**:

| 입력 형식 | 변환 대상 | transpiler |
|---|---|---|
| Bash script (`.bash`, shebang `#!/bin/bash` 등) | `.bashrc`, `.bash_profile`, 일반 bash script | `bash-script` |
| inputrc syntax | `.inputrc` → kash fish-style `bind` | `inputrc` |
| bash-completion files | `/usr/share/bash-completion/completions/*` → kash fish-style `complete` | `bash-completion` |

⚠️ **POSIX sh** (`.sh`, `#!/bin/sh`) 및 **ksh93 script** (`#!/bin/ksh`)는 **transpile 대상이 아님**. 각각 `sh`/`ksh` *personality symlink* (posix-strict/ksh93u-strict mode + 해당 CLI)로 native 실행. personality symlink는 transpiler가 아니라 kash 자체를 호환 모드로 실행하는 alias.

호출 형태 잠정 (별도 commit):
- (a) 단일 도구 + subcommand: `kash-transpile script ...`, `kash-transpile inputrc ...`, `kash-transpile completion ...`
- (b) **별개 도구 multicall** (추천): `bash2kash`, `inputrc2kash`, `bash-completion2kash`
- (c) auto-detect — 위험, 권장 안 함

## Plugin architecture

Script transpiler에 **plugin 시스템**:

- Core transpiler가 bash → kash 기본 변환
- Plugin이 특정 패턴 추가 변환
- **Plugin 자체는 kash 쉘 스크립트 파일** (`.kash`) — 별도 언어 없음, kash 자체로 작성
- Plugin 위치 (잠정): `/usr/share/kash/transpiler-plugins/*.kash`, `~/.config/kash/transpiler-plugins/*.kash`
- 첫 plugin: **gettext plugin** — bash `$"..."` → `$(gettext "...")` 변환

Plugin interface API 명세는 별도 commit (stability rule).

## Transpiling REPL

bash 입력을 즉석 변환해 kash로 실행. 입력 종류 자동 감지 또는 명시적 모드 전환.

### 호출 (잠정)
```sh
kash --bash-repl
# 또는
kash -i
> .kash.bash-mode on
```

### 동작
- 사용자가 bash 구문 입력
- 내부적으로 적절한 transpile → kash로 실행
- 결과는 일반 REPL과 동일

### 옵션
- `--show-transpile`: 변환된 kash 구문도 표시 (학습용)
- 혼합 입력 허용: bash + kash 동시
- History: 원본 입력 저장

### 모드 vs REPL 관계

- transpiling REPL은 *입력 layer*의 변환, mode와 직교
- 변환된 kash 코드는 default 모드에서 실행
- POSIX-strict 모드에서는 transpiling REPL 비활성

## 미결

- Transpiler 구현 위치 (kash 바이너리 내장 vs 별도 `bash2kash` 도구)
- REPL의 line editor 통합 (kash의 zle-equivalent 결정 후 확정)
- bash-specific 변수 (`BASH_SOURCE`, `BASH_LINENO`, `FUNCNAME` 등) kash 대응 카탈로그
- 향후 zsh→kash transpiler 추가 여부 (v1 보류)
