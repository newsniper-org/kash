---
name: kash — bash→kash transpiler and transpiling REPL (committed)
description: bash 4.3+ 스크립트를 kash로 변환하는 transpiler 도구, 그리고 bash 입력을 즉석 변환해 실행하는 REPL 모드
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘의 *이식성 도구* 결정.

## 쉘 이름 확정 — **kash**

지금까지 "신규 쉘"로 부르던 것의 정식 이름은 **kash**. project 디렉토리 `/home/ybi/kash` 정합. 향후 모든 문서/메모리/실행파일 이름에 일관 적용.

**작명 의도**: "Korn Again SHell" — bash("Bourne Again SHell")의 형식을 차용해 *ksh의 계승*임을 직접 표명. ksh93u+m 문법 baseline 선택과 정합. bash → kash transpiler 제공 의도와도 의미적 대칭 (bash 사용자가 자연스럽게 옮겨갈 수 있는 ksh-계열 차세대 셸).

- 정규 호출명: `kash`
- 심볼릭 링크: `sh` (posix-strict + POSIX sh CLI), `ksh` (ksh93u-strict + ksh93u+m CLI)
- bash 호환은 *symlink가 아니라 transpiler*로 제공.

## Transpiler

### 입력 / 출력
- **입력**: bash 4.3 이상 스크립트
- **출력**: kash 스크립트 (default 모드에서 실행 가능)
- 구현은 별도지만 *semantic mapping*은 설계 사항

### 호출 (잠정)
```sh
kash --transpile script.bash > script.kash
kash --transpile --idiomatic script.bash > script.kash
```

### Mapping 전략 — 두 모드

1. **Minimal rewrite (기본)**: 의미가 다른 구문만 최소한으로 변경. 가능한 한 원본 그대로.
2. **Idiomatic (--idiomatic 플래그)**: kash 관용구로 적극 변환 (`${var,,}` → `${(L)var}` 등).

### 변환 필수 항목 (의미 충돌)

| bash 구문 | kash 변환 |
|---|---|
| `cmd1 \|& cmd2` (stderr+stdout pipe) | `cmd1 2>&1 \| cmd2` |
| `BASH_REMATCH[i]` | `.sh.match[i]` |
| `BASHPID` | kash equivalent (미정 — `${.sh.pid}` 같은 형태) |
| `BASH_VERSION` | `${.sh.version}` |
| `shopt -s xxx` | `set -o xxx` 또는 `mode` 선언 |
| `/dev/tcp/HOST/PORT` (in redirect/read) | `tcp-connect HOST PORT` 또는 적절한 coproc wrapping |
| `/dev/udp/HOST/PORT` (in redirect/read) | `udp-send`/`udp-recv` 매핑 |

### 변환 권장 항목 (--idiomatic flag로)

| bash | kash idiomatic |
|---|---|
| `${var,,}` lowercase | `${(L)var}` |
| `${var^^}` uppercase | `${(U)var}` |
| `mapfile -t arr < file` | `arr=( ${(f)"$(<file)"} )` 또는 kash native form |
| `printf -v var fmt args` | `var=$(printf fmt args)` 또는 kash equivalent |
| `[[ -v var ]]` | `[[ -n ${var+x} ]]` 또는 kash equivalent |
| `declare -A` | `typeset -A` |
| `declare -n` | `typeset -n` |

### Unmappable 항목 처리
- **warn-and-best-effort**: 변환 불가능한 구문 (e.g., bash-specific BASH_REMATCH array의 sparse 특이성) 만나면 warning 출력 + 최대한 유사하게 변환
- `--strict` 플래그로 unmappable 만나면 error로 격상
- Mapping 불확실한 구문에는 `# TRANSPILER: <설명>` 주석 자동 삽입

### 호환성 보장 범위
- bash 4.3+ (associative arrays, `[[ -v ]]`, `mapfile`, named `coproc` 등이 모두 도입된 baseline)
- bash 5.x 추가 기능도 가능한 한 매핑
- bash 3.x 이하는 미지원 (배열 동작 등 차이 큼)

## Transpiling REPL

bash 입력을 즉석 변환해 kash로 실행하는 interactive mode.

### 호출 (잠정)
```sh
kash --bash-repl
# 또는 인터랙티브 모드에서 transpiler 토글
kash -i
> .kash.bash-mode on
```

### 동작
- 사용자가 bash 구문 입력
- 내부적으로 transpile → kash로 실행
- 결과는 일반 REPL과 동일

### 옵션
- `--show-transpile`: 변환된 kash 구문도 함께 표시 (학습용)
- 혼합 입력 허용: bash 구문과 kash 구문 동시에 받음 (heuristic로 dispatch)
- History: 원본 입력 (사용자가 친 그대로) 저장

### 모드 vs REPL 관계
- transpiling REPL은 *입력 layer*의 변환, mode와는 직교
- 변환된 kash 코드는 default 모드 (또는 사용자 지정) 에서 실행
- POSIX-strict 모드에서는 transpiling REPL 비활성 (확장 사용 불가능하므로 의미 없음)

## Transpiler 책임 범위 확장 (committed)

bash 호환은 **runtime shim 없이 전적으로 transpiler가 담당** 정책 채택 (project_kash_interactive.md). 입력 문법별로 **별개 transpiler** 필요 (한 도구에 통합한 subcommand로 분기 가능):

| 입력 형식 | 변환 대상 | transpiler |
|---|---|---|
| **Bash script** (`.bash`, shebang `#!/bin/bash` 또는 `#!/usr/bin/env bash`) | `.bashrc`, `.bash_profile`, 일반 bash script body | `bash-script` transpiler |
| **inputrc** (`.inputrc` syntax: `"\C-x\C-r": re-read-init-file` 등) | bash readline key bindings → kash fish-style `bind` | `inputrc` transpiler |
| **bash-completion files** (`complete -F func cmd`, `compgen` 사용) | `/usr/share/bash-completion/completions/*` 등 → kash fish-style `complete` | `bash-completion` transpiler |

⚠️ **POSIX sh script** (`.sh`, shebang `#!/bin/sh` 또는 `#!/usr/bin/env sh`)는 **transpile 대상이 아님**. `sh` personality symlink (posix-strict mode + POSIX CLI)가 native로 실행. `sh`는 transpiler가 아니라 *kash의 POSIX 인격(personality)* 임. 마찬가지로 ksh93 script도 `ksh` symlink로 native 실행.

세 transpiler 모두:
- transpiling REPL은 입력 종류 자동 감지 또는 명시적 모드 전환으로 적절한 transpiler 호출
- 변환 결과는 kash canonical form (fish-style for completion/bind, default mode for script)

### 호출 형태 (잠정 — 별도 commit 필요)

옵션:
- (a) 단일 도구 + subcommand: `kash-transpile script file.bash`, `kash-transpile inputrc .inputrc`, `kash-transpile completion file`
- (b) 별개 도구 (multicall): `bash2kash`, `inputrc2kash`, `bash-completion2kash` — symlink 방식, first-party utility 패턴과 정합
- (c) 단일 도구 + auto-detect (위험 — 모호한 경우 silent 잘못 변환 가능)

내 추천 (잠정): (b) — 명시적이고 multicall 패턴 일관. 단 첫 두 단어 (`bash2kash`)는 약간 길음, naming convention (`<domain>-<action>`) 따르면 `bash-to-kash` 또는 `kash-from-bash` 등 검토.

**정확한 naming/interface는 별도 commit 필요 (stability rule 적용).**

## Plugin architecture (committed)

Script transpiler에 **plugin 시스템** 도입.

- Core transpiler가 bash → kash 기본 변환
- Plugin이 특정 패턴 추가 변환
- **Plugin 자체는 kash 쉘 스크립트 파일** (`.kash`) — 별도 언어/형식 없음
  - kash의 모든 표현력 (typeclass, pattern matching, expansion flags 등) 사용 가능
  - Plugin이 kash가 자기 자신을 host로 활용하는 self-bootstrap 패턴
- Plugin 위치 (잠정): `/usr/share/kash/transpiler-plugins/*.kash`, `~/.config/kash/transpiler-plugins/*.kash`
- 첫 plugin: **gettext plugin** — bash `$"..."`을 `$(gettext "...")` 같은 호출로 변환

### Plugin interface (미결 — 별도 commit 필요)
잠정 컨벤션:
- Plugin script가 well-known 함수 정의 (e.g., `.transpiler.plugin.register-pattern`, `.transpiler.plugin.handle`)
- Transpiler가 AST 순회 중 plugin 함수 호출하여 변환 결과 받음
- 정확한 함수 이름, 인자, AST 표현 형식은 stability rule 적용

## 별도 설계 항목 (미결)

- Transpiler 자체 구현 위치 (kash 바이너리 내장 vs 별도 `bash2kash` 도구)
- REPL의 line editor 통합 — rustyline 기반으로 결정 (project_kash_implementation.md)
- bash-specific 변수 (`BASH_SOURCE`, `BASH_LINENO`, `FUNCNAME` 등) 의 kash 대응 변수 카탈로그
- Transpiler가 인식할 수 없는 bash extension 만났을 때의 정책 (warn 수준, error 격상 조건)
- 향후 zsh→kash transpiler 추가 여부 — 가치 있지만 v1 보류
- bash-completion 패키지 변환의 *완성도* 기준 — 100% 가능? 일부 manual 보완?
- Plugin interface API (위 잠정 부분) 의 정확한 명세
- Plugin discovery / load 순서 / conflict 해결

**How to apply:** kash 고유 기능을 새로 도입할 때 bash 등가물이 있다면 transpiler 매핑 후보로 기록. `.sh.*` 시스템 변수 추가 시 bash의 `BASH_*` 변수와의 대응 관계도 함께 고려.
