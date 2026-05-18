---
name: New shell — mode declaration syntax (committed)
description: mode 선언 구체 문법 — `mode` 키워드, 세 가지 form, modifier monotonicity, ${.sh.mode} introspection, sh/ksh symlinks
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 mode 선언 시스템 확정 사항. (관련: project_shell_modes.md)

## 키워드 — `mode`

세 컨텍스트(file/function/block) 모두 `mode` 하나로 통일.
- ksh/bash와 충돌 없음
- zsh의 `emulate`보다 noun-기반 명료성 (`mode default`가 `emulate default`보다 자연스러움)
- 단일 키워드 = 학습 부담 최소

## 세 가지 form

```sh
mode <name>            # (1) unbounded — 현재 scope 끝까지 + 바깥으로도 전파
mode -L <name>         # (2) lexical — 현재 scope에 한정, exit 시 자동 복원
mode <name> { ... }    # (3) block — 즉시 새 블록, 자동 복원 (= { mode -L <name>; ...; })
```

**Form별 권장:**
- File top: `mode <name>` (자연스럽게 file 전체)
- 함수 안: `mode -L <name>` 권장 (caller mode silent 변경 방지). `mode <name>` (unbounded)은 허용하지만 비-idiomatic.
- 블록: `mode <name> { ... }` (가장 명확)

## Mode 이름 format

```
<base>[-<modifier>]*
```

- Base (소문자): `default`, `posix-strict`, `posix-aware`, `ksh93u-strict`
- Modifier (소문자): `-secure`, 향후 `-noglob`, `-noeval` 등
- 예: `default`, `default-secure`, `posix-aware-secure`, `default-secure-noglob`

## Modifier monotonicity

**Inner scope에서 modifier 추가는 가능하나 제거는 불가.** 안전 modifier가 silent하게 풀리는 것 방지.

```sh
mode default-secure {
    mode posix-aware-secure { ... }    # OK — base 변경, -secure 유지
    mode default-secure-noglob { ... } # OK — modifier 추가
    mode default { ... }               # ✗ ERROR — -secure 제거 불가
}
```

Base mode는 자유롭게 변경 가능 (`default` ↔ `posix-aware` 등).

## Shebang

```sh
#!/usr/bin/env <shellname> --mode=<name>
```

- `--mode=<name>` CLI flag로 file scope 초기 모드 설정
- 없으면 `default`
- 파일 내 `mode <name>` declaration이 있으면 shebang을 override (pragma가 더 명시적이므로 우선)

## Symlinks

전통적 쉘 호환을 위해 호출명에 따라 초기 mode 자동 설정 + **CLI 인수 인터페이스도 동일하게 모방**.

| 호출명 | 초기 mode | CLI 호환 대상 |
|---|---|---|
| (정규 이름) | `default` | 신규 쉘 자체 인터페이스 |
| `sh` | `posix-strict` | POSIX sh CLI 정확히 매칭 |
| `ksh` | `ksh93u-strict` | ksh93u+m CLI 정확히 매칭 |

**핵심 제약 — drop-in CLI 호환:**
- `sh`로 호출 시: POSIX sh의 flag set (`-c`, `-s`, `-i`, `-l`, `-` + set-option flags `-aCefhmnuvx` + `-o name` 등)을 *정확히 동일하게* 파싱하고 동작.
- `ksh`로 호출 시: ksh93u+m의 flag set 전체 (`-A`, `-D`, `-E`, `-h`, `-i`, `-l`, `-m`, `-p`, `-P`, `-r`, `-R`, `-s`, `-t` + 모든 set-option, `-o vi/emacs/tabcomplete/nolog/...` 등)을 정확히 동일하게.
- 새 쉘 고유 flag (`--mode=` 등)는 symlink 호출 시 **거부** — 호환 약속을 깨지 않기 위해. (대안: 무시? 거부가 더 strict — drop-in 의미에 부합.)
- 신규 쉘의 정규 이름으로 호출 시에만 새 flag set 전체 사용 가능.

이 제약은 단순한 mode 초기화 이상의 implementation 비용을 의미함 — 호환 대상별로 CLI parser configuration이 필요하고, flag 의미론 (예: `-h`의 의미가 POSIX/ksh/bash마다 다름)도 정확히 매칭해야 함.

bash/zsh symlink는 별도 호환 mode가 없으므로 미제공.

## Runtime introspection

```sh
${.sh.mode}              # 현재 mode 문자열 (예: "default-secure")
${.sh.mode.base}         # "default", "posix-strict", ...
${.sh.mode.modifiers}    # array — ("secure" "noglob")
```

`.sh.mode`를 compound var로 둠 — ksh93의 `.sh.*` reserved namespace 정합 + compound 구조로 분해 접근 자연스러움.

## 기본값

- Script (no shebang flag, no pragma, no symlink): `default`
- Interactive shell: `default`
- Sourced file: caller mode 상속 (file scope이지만 lexically inner)

## 유효성

- 알 수 없는 mode 이름 → parse error
- 중복 modifier (`default-secure-secure`) → parse error
- Base 없이 modifier만 (`-secure`) → parse error
- modifier monotonicity 위반 → parse/runtime error

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `mode <name>` (unbounded) | × | ✓ | × | ✓ | ✓ |
| `mode -L <name>` | × | ✓ | × | ✓ | ✓ |
| `mode <name> { ... }` | × | ✓ | × | ✓ | ✓ |
| `${.sh.mode}` introspection | × | ✓ | ✓ | ✓ | ✓ |

POSIX-strict와 ksh93u-strict는 `mode` 키워드 자체 비활성 — strict 모드는 정의상 다른 mode로 escape 불가. escape 필요하면 한 단계 위에서 mode 결정.

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** mode 시스템 후속 결정 (modifier 추가, mode 간 default 의미론 등) 시 이 syntax를 baseline으로. 모든 모드 관련 메모리(`project_shell_modes.md`)와 일관 유지.
